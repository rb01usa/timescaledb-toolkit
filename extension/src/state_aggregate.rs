//! SELECT duration_in('STOPPED', states) as run_time, duration_in('ERROR', states) as error_time FROM (
//!   SELECT state_agg(time, state) as states FROM ...
//! );

#![allow(non_camel_case_types)]

use serde::{Deserialize, Serialize};
use std::slice;

use aggregate_builder::aggregate;

use flat_serialize::*;
use flat_serialize_macro::FlatSerializable;
use pgx::*;

use std::convert::TryInto as _;

use crate::{
    accessors::toolkit_experimental,
    aggregate_utils::in_aggregate_context,
    flatten,
    palloc::{Inner, Internal, InternalAsValue, ToInternal},
    pg_type,
    raw::bytea,
    ron_inout_funcs,
};

use crate::raw::TimestampTz;

// Intermediate state kept in postgres.  This is a tdigest object paired
// with a vector of values that still need to be inserted.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct StateAggTransState {
    last_state: Option<(String, i32)>,
    // TODO using BTreeMap so lazy prototype tests can depend on stable iteration order
    durations: std::collections::BTreeMap<String, i32>,
}

impl StateAggTransState {
    fn new() -> Self {
        Self {
            last_state: None,
            durations: std::collections::BTreeMap::new(),
        }
    }

    fn record(&mut self, state: String, time: crate::raw::TimestampTz) {
        let raw_time: i64 = time.into();
        // XXX truncate because i don't know how to operate pg_type yet
        let raw_time = (raw_time / 1_000_000) as i32;
        if let Some((last_state, last_time)) = self.last_state.take() {
            if last_state == state {
                // Same state recorded twice.  Do nothing so as to preserve the older timestamp.
                // TODO Do we care?  Could be that states are recorded not only when they change but
                // also at regular intervals even when they don't?
                todo!("add test covering this: {} recorded twice:  ignoring {} in favor of previously recorded {}",
                      state, raw_time, last_time);
                self.last_state = Some((state, raw_time));
            } else {
                self.last_state = Some((state.clone(), raw_time));
                match self.durations.get_mut(&last_state) {
                    None => {
                        println!(
                            "state changed from {} to {}, previously unseen, storing time={} - last_time={} = {}",
                            last_state, state, raw_time, last_time, raw_time - last_time
                        );
                        // TODO dedup these arms
                        self.durations.insert(last_state, raw_time - last_time);
                    }
                    Some(duration) => {
                        let this_duration = raw_time - last_time;
                        let new_duration = *duration + this_duration;
                        println!("state changed from {} to {}, storing duration={} + time={} - last_time={} = {}",
                                  last_state, state, duration, raw_time, last_time, new_duration);
                        *duration = new_duration;
                    }
                }
            }
        } else {
            println!("first state is {} {}", state, raw_time);
            self.last_state = Some((state, raw_time));
        }
    }
}

#[pg_extern(immutable)]
fn state_agg_trans(
    state: Internal,
    time: crate::raw::TimestampTz,
    value: Option<String>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Internal {
    let inner = unsafe { state.to_inner() };
    unsafe { in_aggregate_context(fcinfo, || state_agg_inner(inner, time, value)) }.internal()
}

fn state_agg_inner(
    state: Option<Inner<StateAggTransState>>,
    time: crate::raw::TimestampTz,
    value: Option<String>,
) -> Option<Inner<StateAggTransState>> {
    let value = match value {
        None => return state,
        Some(value) => value,
    };
    let mut state = match state {
        None => StateAggTransState::new().into(),
        Some(state) => state,
    };
    state.record(value, time);
    Some(state)
}

#[pg_extern(immutable)]
fn state_agg_final(state: Internal, fcinfo: pg_sys::FunctionCallInfo) -> Option<StateAgg<'static>> {
    let inner = unsafe { state.to_inner() };
    unsafe { in_aggregate_context(fcinfo, || state_agg_final_inner(inner)) }
}
fn state_agg_final_inner(state: Option<Inner<StateAggTransState>>) -> Option<StateAgg<'static>> {
    let durations = state
        .map(|s| {
            s.durations
                .iter()
                .map(|(state, duration)| {
                    DurationInState {
                        duration: *duration as i32,
                        // TODO testing hack.  need to figure out how to get the HashMap into StateAgg
                        state: match state.as_str() {
                            "one" => 1,
                            "two" => 2,
                            "three" => 3,
                            _ => todo!("this is just a hack for prototyping"),
                        },
                    }
                })
                .collect()
        })
        .unwrap_or_else(Vec::new);
    Some(unsafe {
        flatten!(StateAgg {
            len: durations.len() as u64,
            durations: (&*durations).into(),
        })
    })
}

extension_sql!(
    "CREATE AGGREGATE state_agg( ts timestamptz, state text )\n\
     (\n\
     sfunc = state_agg_trans,\n\
     stype = internal,\n\
     finalfunc = state_agg_final,\n\
     parallel = restricted\n\
     );\n",
    name = "state_agg",
    requires = [state_agg_trans, state_agg_final],
);

// TODO my struggles with pg_type
// - tl;dr - How do you use it???
// - putting anything other than two 32-bit types in here just gives me
//	assertion failed: `(left == right)`
//	  left: `8`,
//	 right: `0`
//   I tried a lot of things but couldn't get the size and/or alignment correct.
// - Is this type even the right move?  Can I or shoudl I put the HashMap directly into StateAgg instead?
#[derive(Clone, Debug, Deserialize, FlatSerializable, PartialEq, Serialize)]
#[repr(C)]
pub struct DurationInState {
    duration: i32,
    // TODO String not FlatSerializable... what do?
    //state: String,
    state: u32,
}

pg_type! {
    #[derive(Debug)]
    struct StateAgg<'input> {
        len: u64,
        durations: [DurationInState; self.len],
    }
}

ron_inout_funcs!(StateAgg);

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use super::*;
    use pgx::*;
    use pgx_macros::pg_test;
    macro_rules! select_one {
        ($client:expr, $stmt:expr, $type:ty) => {
            $client
                .select($stmt, None, None)
                .first()
                .get_one::<$type>()
                .unwrap()
        };
    }

    #[pg_test]
    fn one_state_one_change() {
        Spi::execute(|client| {
            client.select("CREATE TABLE test(ts timestamptz, state TEXT)", None, None);
            client.select(
                r#"INSERT INTO test VALUES
                    ('2020-01-01 00:00:00+00', 'one'),
                    ('2020-12-31 00:02:00+00', 'end')
                "#,
                None,
                None,
            );

            let stmt = "SELECT state_agg(ts, state) FROM test";
            let a = select_one!(client, stmt, StateAgg);
            assert_eq!(1, a.len);
            assert_eq!(
                flat_serialize::Slice::Slice(&[DurationInState {
                    duration: 31536120,
                    state: 1,
                }]),
                a.durations
            );
        });
    }

    #[pg_test]
    fn two_states_two_changes() {
        Spi::execute(|client| {
            client.select("CREATE TABLE test(ts timestamptz, state TEXT)", None, None);
            client.select(
                r#"INSERT INTO test VALUES
                    ('2020-01-01 00:00:00+00', 'one'),
                    ('2020-01-01 00:01:00+00', 'two'),
                    ('2020-12-31 00:02:00+00', 'end')
                "#,
                None,
                None,
            );

            let stmt = "SELECT state_agg(ts, state) FROM test";
            let a = select_one!(client, stmt, StateAgg);
            assert_eq!(2, a.len);
            assert_eq!(
                flat_serialize::Slice::Slice(&[
                    DurationInState {
                        duration: 60,
                        state: 1,
                    },
                    DurationInState {
                        duration: 31536060,
                        state: 2,
                    }
                ]),
                a.durations
            );
        });
    }

    #[pg_test]
    fn two_states_three_changes() {
        Spi::execute(|client| {
            client.select("CREATE TABLE test(ts timestamptz, state TEXT)", None, None);
            client.select(
                r#"INSERT INTO test VALUES
                    ('2020-01-01 00:00:00+00', 'one'),
                    ('2020-01-01 00:01:00+00', 'two'),
                    ('2020-01-01 00:02:00+00', 'one'),
                    ('2020-12-31 00:02:00+00', 'end')
                "#,
                None,
                None,
            );

            let stmt = "SELECT state_agg(ts, state) FROM test";
            let a = select_one!(client, stmt, StateAgg);
            assert_eq!(2, a.len);
            assert_eq!(
                flat_serialize::Slice::Slice(&[
                    DurationInState {
                        duration: 31536060,
                        state: 1,
                    },
                    DurationInState {
                        duration: 60,
                        state: 2,
                    }
                ]),
                a.durations
            );
        });
    }
}
