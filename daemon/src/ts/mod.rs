// Copyright 2017-2020 Aron Heinecke
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::config::TSConfig;
use crate::db;
use crate::error::Error;
use crate::Result;
use crate::*;
use connection::Connection;
use mysql::{Pool, PooledConn};
use std::collections::HashMap;
use std::collections::HashSet;
use std::convert::TryInto;
use std::thread::{self, sleep};
use std::time::Duration;
use std::{sync::RwLock, time::Instant};
use timer::*;
use ts3_query::*;

const CHANNEL_NAME: &str = "channel_name";
const CHANNEL_ID: &str = "cid";

const CLIENT_TYPE: &str = "client_type";
const CLIENT_TYPE_NORMAL: &str = "0";
/// Per-Identity ID
const CLIENT_DB_ID: &str = "client_database_id";
/// Per-connection ID
const CLIENT_CONN_ID: &str = "clid";
const CLIENT_GROUPS: &str = "client_servergroups";
const CLIENT_CHANNEL: &str = CHANNEL_ID;
const CLIENT_NAME: &str = "client_nickname";

/// Safety: connection timeout has to be short enough that no request blocks others!
const INTERVAL_ACTIVITY_S: i64 = 30;
const INTERVAL_FLUSH_M: i64 = 15;
const COOLDOWN_POKE_S: u64 = 60 * 20;

mod connection;

/// Holds TS statistics data and cleans up on drop
///
/// Is used by daemon to hold user activity.
struct TsStatCtrl {
    pool: Pool,
    conn: Connection,
    names: HashMap<TsClDBID, String>,
    times: HashMap<(TsClDBID, TsChannelID), i32>,
    last_channel: HashMap<TsClDBID, TsChannelID>,
    last_date: chrono::naive::NaiveDate,
    last_update: Instant,
    last_guest_poke: Option<Instant>,
    /// none also if no poke enabled
    poke_config: Option<PokeConfig>,
}

#[derive(Default, Debug)]
struct PokeConfig {
    poke_group: TsGroupID,
    guest_group: TsGroupID,
    guest_channel: TsGroupID,
    poke_msg: String,
}

impl TsStatCtrl {
    fn new(pool: Pool, config: Config) -> Result<Self> {
        let mut data = Self {
            pool,
            conn: Connection::new(config)?,
            last_date: Local::today().naive_local(),
            last_update: Instant::now(),
            names: Default::default(),
            times: Default::default(),
            last_channel: Default::default(),
            last_guest_poke: Default::default(),
            poke_config: Default::default(),
        };
        data.inner_update_settings(true)?;
        Ok(data)
    }

    /// Update dynamic settings from DB
    pub fn update_settings(&mut self) -> Result<()> {
        self.inner_update_settings(false)
    }

    /// Update dynamic settings from DB. first_run controls log-output
    fn inner_update_settings(&mut self, first_run: bool) -> Result<()> {
        // shouldn't name Pool & TsConnection conn..
        let old_enabled = self.poke_config.is_some();
        let mut conn = self.pool.get_conn()?;
        self.poke_config = if is_ts3_guest_check_enabled(&mut conn, &self.conn.config()) {
            read_poke_config(&mut conn)?
        } else {
            None
        };
        if self.poke_config.is_some() != old_enabled || first_run {
            let msg = if self.poke_config.is_some() {
                "Ts-Poke enabled"
            } else {
                "Ts-Poke disabled"
            };
            db::log_message(&mut conn, msg, "unable to log poke config change");
        }
        Ok(())
    }

    /// Check online clients, update activity & send notifications
    fn tick(&mut self) -> Result<()> {
        // store timestamp now to prevent delta loss by blocking operations
        let new_timestamp = Instant::now();
        let data = get_online_clients(&mut self.conn)?;
        // take elapsed after data, expecting server reponse to be fast and connection start possibly slow
        let elapsed: i32 = match self.last_update.elapsed().as_secs().try_into() {
            Ok(v) => v,
            Err(e) => panic!("TS activity elapsed time > i32::max! {}", e),
        };
        trace!("Elapsed: {} seconds", elapsed);
        self.last_update = new_timestamp;

        let mut new_channel = HashMap::with_capacity(data.len());

        let mut poke_clients: Vec<TsConID> = Vec::new();
        // new guest
        let mut new_guest = false;
        // poke-receiver at place
        let mut has_lt = false;
        let poke_cfg = if self
            .last_guest_poke
            .map_or(false, |v| v.elapsed().as_secs() > COOLDOWN_POKE_S)
        {
            self.poke_config.as_ref()
        } else {
            None
        };

        for client in data {
            let id = client.cldbid;
            self.names.insert(id, client.name);
            // add elapsed time to last channel, or current if no previous is known
            let chan = self.last_channel.get(&id).unwrap_or(&client.channel);

            let k = (id, *chan);
            if let Some(time) = self.times.get_mut(&k) {
                *time += elapsed;
            } else {
                self.times.insert(k, elapsed);
            }

            if let Some(cfg) = poke_cfg {
                if !has_lt {
                    if cfg.guest_channel == client.channel {
                        if client.groups.contains(&cfg.guest_group) {
                            new_guest = true;
                        } else if client.groups.contains(&cfg.guest_group) {
                            has_lt = true;
                        }
                    } else if client.groups.contains(&cfg.poke_group) {
                        poke_clients.push(client.conid);
                    }
                }
            }

            // remember current channel for next time
            new_channel.insert(id, client.channel);
        }

        // excludes disconnected clients
        self.last_channel = new_channel;

        if new_guest && !has_lt {
            self.poke_clients(poke_clients)?;
        } else if has_lt {
            // reset cooldown time if poke-receiver at place
            self.last_guest_poke = Some(Instant::now());
        }
        Ok(())
    }

    /// poke clients, runs new thread
    fn poke_clients(&mut self, clients: Vec<TsConID>) -> Result<()> {
        self.last_guest_poke = Some(Instant::now());
        let cooldown = match self.conn.config().ts.cmd_limit_secs {
            0 => None,
            // subtract
            v => Some(Duration::from_millis(1000 / (v as u64))),
        };
        let msg: &str = match self.poke_config.as_ref().map(|v| &v.poke_msg) {
            Some(v) => v,
            None => unreachable!("Unreachable! poke_clients invoked without poke_config!"),
        };
        let escaped = raw::escape_arg(msg);
        let mut conn = self.conn.clone()?;
        thread::Builder::new()
            .name("ts-poke".to_string())
            .spawn(move || {
                let res = || -> Result<()> {
                    for client in clients {
                        if let Some(cooldown) = cooldown {
                            sleep(cooldown);
                        }
                        conn.get()?
                            .raw_command(&format!("poke clid={} msg={}", client, &escaped))?;
                    }
                    Ok(())
                };
                if let Err(e) = res() {
                    error!("Guest-Notification failed: {}", e);
                }
            })
            .expect("Can't spawn poke-thread!");
        Ok(())
    }

    /// Flush data to DB
    fn flush_data(&mut self) -> Result<()> {
        let mut conn = self.pool.get_conn()?;
        // clear data only after successful update
        let values: Vec<(TsClDBID, &str)> = self
            .names
            .iter()
            .map(|(id, name)| (*id, name.as_str()))
            .collect();
        db::ts::update_ts_names(&mut conn, values.as_slice())?;
        trace!("Flushed {} name entries", self.names.len());
        self.names.clear();

        let values: Vec<_> = self
            .times
            .iter()
            .map(|((client, channel), time)| TsActivity {
                client: *client,
                channel: *channel,
                time: *time,
            })
            .collect();
        db::ts::update_ts_activity(&mut conn, self.last_date, values.as_slice())?;
        trace!("Flushed {} time entries", self.times.len());
        self.times.clear();
        self.last_date = Local::today().naive_local();
        Ok(())
    }
}

fn read_poke_config(conn: &mut PooledConn) -> Result<Option<PokeConfig>> {
    let poke_group = db::read_setting(conn, crate::TS3_GUEST_WATCHER_GROUP_KEY)?;
    let guest_group = db::read_setting(conn, crate::TS3_GUEST_GROUP_KEY)?;
    let guest_channel = db::read_setting(conn, crate::TS3_GUEST_CHANNEL_KEY)?;
    let poke_msg = db::read_string_setting(conn, crate::TS3_GUEST_POKE_MSG)?
        .unwrap_or_else(|| "Guest arrived!".to_string());

    Ok(
        if let (Some(poke_group), Some(guest_group), Some(guest_channel)) =
            (poke_group, guest_group, guest_channel)
        {
            Some(PokeConfig {
                poke_group,
                guest_group,
                guest_channel,
                poke_msg,
            })
        } else {
            None
        },
    )
}

/// Timer & data guard, ensures TS data write on drop
pub struct TsGuard {
    _timer: Vec<Timer>,
    _task_guards: Vec<Guard>,
    stats: Arc<RwLock<TsStatCtrl>>,
}

impl Drop for TsGuard {
    fn drop(&mut self) {
        let mut stats = self.stats.write().unwrap();
        if let Err(e) = stats.flush_data() {
            error!("Flushing data on exit: {}", e);
            eprintln!("Flushing data on exit: {}", e);
        }
    }
}

/// Start TS daemon, returns scheduler-guards
pub fn start_daemon(pool: Pool, config: Config) -> Result<Option<TsGuard>> {
    if config.ts.enabled {
        debug!("Starting TS activity check");
        let timer_1 = Timer::new();
        // TODO: better threading sync, blocks ticks on flush
        let ts_handler = Arc::new(RwLock::new(TsStatCtrl::new(pool.clone(), config.clone())?));
        let handler_c = ts_handler.clone();
        let guard_1 =
            timer_1.schedule_repeating(chrono::Duration::seconds(INTERVAL_ACTIVITY_S), move || {
                trace!("Performing ts handler tick");
                let mut guard = handler_c.write().unwrap();
                if let Err(e) = guard.tick() {
                    error!("Ticking ts-activity: {}", e);
                }
            });
        let timer_2 = Timer::new();
        let handler_c = ts_handler.clone();
        let guard_2 =
            timer_2.schedule_repeating(chrono::Duration::minutes(INTERVAL_FLUSH_M), move || {
                trace!("Performing channel update & data flush");
                {
                    let mut guard = handler_c.write().unwrap();
                    if let Err(e) = guard.flush_data() {
                        error!("Flushing TS Data to DB! {}", e);
                    }
                    if let Err(e) = guard.update_settings() {
                        error!("Updating TS config from DB! {}", e);
                    }
                }
                if let Err(e) = Connection::new(config.clone())
                    .map(|mut conn| update_channels(&pool, &mut conn))
                {
                    error!("Performing TS channel update! {}", e);
                }
            });

        Ok(Some(TsGuard {
            _timer: vec![timer_1, timer_2],
            _task_guards: vec![guard_1, guard_2],
            stats: ts_handler,
        }))
    } else {
        info!("TS activity check disabled, guest-poke disabled");
        Ok(None)
    }
}

/// Error-Wrapper for updating TS channel list
fn update_channels(pool: &Pool, mut conn: &mut Connection) -> Result<()> {
    db::ts::upsert_channels(&mut pool.get_conn()?, &get_channels(&mut conn)?)?;
    Ok(())
}

/// Check for unknown identities with member group and update unknown_ts_ids
pub fn find_unknown_identities(pool: &Pool, ts_cfg: &TSConfig) -> Result<()> {
    let retry_time_secs = 60;
    let max_tries = 10;
    let mut conn = pool.get_conn()?;
    for i in 1..=max_tries {
        // don't retry if DB is missing values, only on DB connection problems
        let group_ids =
            get_ts3_member_groups(&mut conn)?.ok_or(Error::MissingKey(crate::TS3_MEMBER_GROUP))?;
        match find_unknown_inner(&group_ids, &mut conn, ts_cfg) {
            Ok(_) => return Ok(()),
            Err(e) => {
                if i == max_tries {
                    error!("Retried unknown-identity-check {} times, aborting.", i);
                    return Err(e);
                } else {
                    warn!("Failed unknown-identity-check, try no {}, error: {}", i, e);
                    sleep(Duration::from_secs(retry_time_secs * i));
                }
            }
        }
    }
    unreachable!();
}

pub fn print_poke_config(conn: &mut PooledConn, config: &Config) -> Result<String> {
    Ok(if is_ts3_guest_check_enabled(conn, config) {
        let cfg = read_poke_config(conn)?;
        match cfg {
            Some(v) => format!("Guest-Poke enabled: {:?}", v),
            None => format!("Guest-Poke enabled but missing fields!"),
        }
    } else {
        String::from("Guest-Poke disabled")
    })
}

// use try {} when #31436 is stable
fn find_unknown_inner(
    group_ids: &[usize],
    mut conn: &mut PooledConn,
    ts_cfg: &TSConfig,
) -> Result<()> {
    trace!("Connect ts3");
    let mut connection = QueryClient::new(format!("{}:{}", ts_cfg.ip, ts_cfg.port))?;
    trace!("login");
    connection.login(&ts_cfg.user, &ts_cfg.password)?;
    trace!("server select");
    connection.select_server_by_port(ts_cfg.server_port)?;
    trace!("TS3 server connection ready");

    let mut ids = Vec::new();
    for group in group_ids {
        ids.append(&mut connection.get_servergroup_client_list(*group)?);
        trace!("Retrieved ts clients for {}", group);
    }
    db::ts::update_unknown_ts_ids(&mut conn, &ids)?;

    debug!("Performed TS identity check. {} IDs", ids.len());
    Ok(())
}

/// Get ts3 member groups settings, return an optional vec of member group-ids
#[inline]
pub fn get_ts3_member_groups(conn: &mut PooledConn) -> Result<Option<Vec<usize>>> {
    db::read_list_setting(conn, crate::TS3_MEMBER_GROUP)
}

/// Read ts3 unknown identity settings from DB
fn is_ts3_guest_check_enabled(conn: &mut PooledConn, config: &Config) -> bool {
    match db::read_bool_setting(conn, TS3_GUEST_NOTIFY_ENABLE_KEY) {
        Ok(Some(v)) => v,
        Ok(None) => config.ts.unknown_id_check_enabled,
        Err(e) => {
            error!(
                "Error retrieving '{}' setting: {}",
                TS3_GUEST_NOTIFY_ENABLE_KEY, e
            );
            config.ts.unknown_id_check_enabled
        }
    }
}

/// Get clients on ts. Returns last entry for multiple connection of same ID.
fn get_online_clients(conn: &mut Connection) -> Result<HashSet<TsClient>> {
    let res = raw::parse_multi_hashmap(conn.get()?.raw_command("clientlist -groups")?, false);
    //dbg!(res.len());
    // dbg!(&res);
    let clid_str = conn.conn_id()?.to_string();
    let clients = res
        .into_iter()
        .filter(|e| {
            e.get(CLIENT_TYPE).map(String::as_str) == Some(CLIENT_TYPE_NORMAL)
                || e.get(CLIENT_CONN_ID) == Some(&clid_str)
        })
        .map(|e| {
            Ok(TsClient {
                name: e
                    .get(CLIENT_NAME)
                    .map(raw::unescape_val)
                    .ok_or_else(|| Error::TsMissingValue(CLIENT_NAME))?,
                cldbid: e
                    .get(CLIENT_DB_ID)
                    .ok_or_else(|| Error::TsMissingValue(CLIENT_DB_ID))?
                    .parse()?,
                conid: e
                    .get(CLIENT_CONN_ID)
                    .ok_or_else(|| Error::TsMissingValue(CLIENT_CONN_ID))?
                    .parse()?,
                channel: e
                    .get(CLIENT_CHANNEL)
                    .ok_or_else(|| Error::TsMissingValue(CLIENT_CHANNEL))?
                    .parse()?,
                groups: e
                    .get(CLIENT_GROUPS)
                    .ok_or_else(|| Error::TsMissingValue(CLIENT_GROUPS))?
                    .split(',')
                    .map(|e| e.parse().map_err(From::from))
                    .collect::<Result<Vec<_>>>()?,
            })
        })
        .collect::<Result<HashSet<TsClient>>>()?;
    Ok(clients)
}

fn get_channels(conn: &mut Connection) -> Result<Vec<Channel>> {
    let res = raw::parse_multi_hashmap(conn.get()?.raw_command("channellist")?, false);
    res.into_iter()
        .map(|e| {
            Ok(Channel {
                id: e
                    .get(CHANNEL_ID)
                    .ok_or_else(|| Error::TsMissingValue(CHANNEL_ID))?
                    .parse()?,
                name: e
                    .get(CHANNEL_NAME)
                    .map(raw::unescape_val)
                    .ok_or_else(|| Error::TsMissingValue(CHANNEL_NAME))?,
            })
        })
        .collect::<Result<Vec<_>>>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{default_cfg_testing, TSConfig};

    #[test]
    #[ignore]
    fn perform_get_online_clients() {
        let ts_cfg = TSConfig {
            ip: option_env!("ts_ip").unwrap_or("localhost").to_string(),
            port: option_env!("ts_port").unwrap_or("11001").parse().unwrap(),
            user: option_env!("ts_user").unwrap_or("serveradmin").to_string(),
            password: option_env!("ts_pw").unwrap_or("1234").to_string(),
            server_port: option_env!("ts_port_server")
                .unwrap_or("6678")
                .parse()
                .unwrap(),
            unknown_id_check_enabled: true,
            enabled: true,
            cmd_limit_secs: 4,
        };
        // create default cfg, change to use our ts config
        let mut def = default_cfg_testing();
        Arc::get_mut(&mut def).unwrap().ts = ts_cfg;
        dbg!(&def);

        let mut conn = Connection::new(def).unwrap();
        let clients = get_online_clients(&mut conn).unwrap();
        dbg!(clients);
        // let channels = get_channels(&mut conn).unwrap();
        // dbg!(channels);
        // let snapshot = conn
        //     .get()
        //     .unwrap()
        //     .raw_command("serversnapshotcreate")
        //     .unwrap();
        // dbg!(snapshot);
    }
}
