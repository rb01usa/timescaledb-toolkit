//! Based on the paper: https://cs.ucsb.edu/sites/default/files/documents/2005-23.pdf

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    mem::size_of
};

use pgx::*;

use pg_sys::{Datum, Oid};

use flat_serialize::*;

use crate::{
    aggregate_utils::{get_collation, in_aggregate_context},
    datum_utils::{DatumHashBuilder, DatumStore, deep_copy_datum},
    ron_inout_funcs,
    build,
    palloc::{Internal, InternalAsValue, Inner, ToInternal}, 
    pg_type,
};

use crate::frequency::toolkit_experimental::{FrequencyAggregate};

// Unable to implement PartialEq for AnyElement, so creating a local copy
struct LocalAnyElement {
    datum: Datum,
    typoid: Oid,
}

impl PartialEq for LocalAnyElement {
    // JOSH TODO should probably store the fn pointer instead of the OID (OID or another fn ptr will also be needed for serialization)
    fn eq(&self, other: &Self) -> bool {
        unsafe {
            if self.typoid != other.typoid {
                false
            } else {
                let typ = self.typoid;
                let tentry =
                    pg_sys::lookup_type_cache(typ, pg_sys::TYPECACHE_EQ_OPR_FINFO as _);
            
                let flinfo = if (*tentry).eq_opr_finfo.fn_addr.is_some() {
                    &(*tentry).eq_opr_finfo
                } else {
                    pgx::error!("no equality function");
                };
        
                // JOSH TODO can do this on stack, ask when ready (I think DatumHasher is doing this)
                let size =
                    size_of::<pg_sys::FunctionCallInfoBaseData>() + size_of::<pg_sys::NullableDatum>() * 2;
                let mut info = pg_sys::palloc0(size) as pg_sys::FunctionCallInfo;
        
                (*info).flinfo = flinfo as *const pg_sys::FmgrInfo as *mut pg_sys::FmgrInfo;
                (*info).context = std::ptr::null_mut();
                (*info).resultinfo = std::ptr::null_mut();
                (*info).fncollation = (*tentry).typcollation;
                (*info).isnull = false;
                (*info).nargs = 2;

                (*info).args.as_mut_slice(2)[0] = pg_sys::NullableDatum {
                    value: self.datum,
                    isnull: false,
                };
                (*info).args.as_mut_slice(2)[1] = pg_sys::NullableDatum {
                    value: other.datum,
                    isnull: false,
                };
                (*(*info).flinfo).fn_addr.unwrap()(info) != 0
            }
        }
    }
}

impl Eq for LocalAnyElement {}

impl Hash for LocalAnyElement {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.datum.hash(state);
    }
}

impl From<AnyElement> for LocalAnyElement {
    fn from(other: AnyElement) -> Self {
        LocalAnyElement {
            datum: other.datum(),
            typoid: other.oid(),
        }
    }
}

#[derive(Clone)]
struct FrequencyEntry{
    value: Datum,
    count: u64,
    overcount: u64,
}

const MIN_SIZE: usize = 10;
const MAX_NOISE_RATIO: f64 = 0.9;

pub struct FrequencyTransState {
    entries: Vec<FrequencyEntry>,
    indicies: HashMap<LocalAnyElement, usize, DatumHashBuilder>,
    total_vals: u64,
    min_freq: f64,
    typ: Oid, // JOSH TODO redundant with the DatumHashBuilder?
    complete: bool,  // true if all seen values in entries
}

impl FrequencyTransState {
    unsafe fn from_type_id(min_freq: f64, typ: pg_sys::Oid, collation: Option<Oid>) -> Self {
        FrequencyTransState {
            entries: vec![],
            indicies: HashMap::with_hasher(DatumHashBuilder::from_type_id(typ, collation)),
            total_vals: 0,
            min_freq,
            typ,
            complete: true,
        }
    }

    fn add(&mut self, element: LocalAnyElement) {
        self.total_vals += 1;
        // JOSH TODO clippy will say "use match" (didn't actually see this), also would entry() API be better?
        if let Some(idx) = self.indicies.get(&element) {
            let idx = *idx;
            self.entries[idx].count += 1;
            self.move_left(idx);
        } else {
            // TODO: might be inefficient to call should_grow on every iteration
            if self.entries.len() < MIN_SIZE || self.should_grow() {
                let new_idx = self.entries.len();
                let overcount = if self.complete { 0 } else { self.entries.last().unwrap().overcount };
                unsafe {
                    self.entries.push(
                        FrequencyEntry {
                            value: deep_copy_datum(element.datum, element.typoid),
                            count: 1 + overcount,
                            overcount,
                        }
                    );
                }
                // Important to create the indices entry using the datum in the local context
                self.indicies.insert(LocalAnyElement{ datum: self.entries[new_idx].value, typoid: self.typ }, new_idx);
            } else {
                self.complete = false;
                let new_value = unsafe { deep_copy_datum(element.datum, element.typoid) };

                // TODO: might be more efficient to replace the lowest indexed tail value (count matching last) and not call move_up
                let entry = self.entries.last_mut().unwrap();
                self.indicies.remove(&LocalAnyElement { datum: entry.value, typoid: self.typ });
                entry.value = new_value; // JOSH FIXME should we pfree() old value if by-ref?
                entry.overcount = entry.count;
                entry.count += 1;
                self.indicies.insert(LocalAnyElement{ datum: new_value, typoid: self.typ }, self.entries.len() - 1);
                self.move_left(self.entries.len() - 1);
            }
        }
    }

    fn should_grow(&self) -> bool {
        let mut used_count = 0;  // This will be the sum of the counts of elements that occur more than min_freq

        let mut i = 0;
        while i < self.entries.len() && self.entries[i].count as f64 / self.total_vals as f64 > self.min_freq {
            used_count += self.entries[i].count - self.entries[i].overcount;  // Would just using count here introduce too much error?
            i += 1
        }

        if i == self.entries.len() {
            true
        } else {
            // At this point the first 'i' entries are all of the elements that occur more than 'min_freq' and account for 'used_count' of all the entries encountered so far.

            // Noise threshold is the count below which we don't track values (this will be approximately the overcount of churning buckets)
            let noise_threhold = self.min_freq * MAX_NOISE_RATIO * self.total_vals as f64;

            // We compute our target size as 'i' plus the amount of buckets the remaining entries could be divided among if there are no values occuring between 'min_freq' and 'noise_threshold'
            let remainder = self.total_vals - used_count;
            let target_size = f64::ceil(remainder as f64 / noise_threhold) as usize + i;

            self.entries.len() < target_size
        }
    }

    // swap element i with an earlier element in the 'entries' vector to maintain decreasing order
    fn move_left(&mut self, i: usize) {
        let count = self.entries[i].count;
        let mut target = i;
        while target > 0 && self.entries[target - 1].count < count {
            target -= 1;
        }
        if target != i {
            self.entries.swap(i, target);

            self.update_map_index(i);
            self.update_map_index(target);
        }
    }

    // Adds the 'indicies' lookup entry for the value at 'entries' index i
    fn update_map_index(&mut self, i: usize) {
        let element_for_i = LocalAnyElement{ datum: self.entries[i].value, typoid: self.typ };
        *self.indicies.get_mut(&element_for_i).unwrap() = i;
    }
}

#[pg_extern(immutable, parallel_safe, schema = "toolkit_experimental")]
pub fn freq_trans(
    state: Internal,
    freq: f64,
    value: Option<AnyElement>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Internal {
    freq_trans_inner(unsafe{ state.to_inner() }, freq, value, fcinfo).internal()
}
pub fn freq_trans_inner(
    state: Option<Inner<FrequencyTransState>>,
    freq: f64,
    value: Option<AnyElement>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<Inner<FrequencyTransState>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            let value = match value {
                None => return state,
                Some(value) => value
            };
            let mut state = match state {
                None => {
                    // JOSH TODO can get from value unless we want to use crate::raw::anyelement
                    let typ = pgx::get_getarg_type(fcinfo, 2);
                    let collation = get_collation(fcinfo);
                    FrequencyTransState::from_type_id(freq, typ, collation).into()
                },
                Some(state) => state,
            };

            state.add(value.into());
            Some(state)
        })
    }
}

#[pg_schema]
pub mod toolkit_experimental {
    pub(crate) use super::*;

    pg_type! {
        #[derive(Debug)]
        struct FrequencyAggregate<'input> {
            type_oid: u32,
            num_values: u32,
            values_seen: u64,
            min_freq: f64,
            counts: [u64; self.num_values], // JOSH TODO look at AoS instead of SoA at some point
            overcounts: [u64; self.num_values],
            datums: DatumStore<'input>,
        }
    }

    impl <'input> From<Internal> for FrequencyAggregate<'input> {
        fn from(trans: Internal) -> Self {
            Self::from(unsafe { trans.to_inner().unwrap() } )
        }
    }

    impl <'input> From<Inner<FrequencyTransState>> for FrequencyAggregate<'input> {
        fn from(trans: Inner<FrequencyTransState>) -> Self {
            let mut values = Vec::new();
            let mut counts = Vec::new();
            let mut overcounts = Vec::new();
            
            for entry in &trans.entries {
                values.push(entry.value);
                counts.push(entry.count);
                overcounts.push(entry.overcount);
            }

            build!{
                FrequencyAggregate {
                    type_oid: trans.typ as _,
                    num_values: trans.entries.len() as _,
                    values_seen: trans.total_vals,
                    min_freq: trans.min_freq,
                    counts: counts.into(),
                    overcounts: overcounts.into(),
                    datums: DatumStore::from((trans.typ, values)),
                }
            }
        }
    }

    ron_inout_funcs!(FrequencyAggregate);
}

// PG function to generate a user-facing TopN object from a InternalTopN.
#[pg_extern(immutable, parallel_safe, schema = "toolkit_experimental")]
fn freq_final(
    state: Internal,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<toolkit_experimental::FrequencyAggregate<'static>> {
    unsafe {
        freq_final_inner(state.to_inner(), fcinfo)
    }
}
fn freq_final_inner(
    state: Option<Inner<FrequencyTransState>>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<toolkit_experimental::FrequencyAggregate<'static>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            let state = match state {
                None => return None,
                Some(state) => state,
            };

            Some(FrequencyAggregate::from(state))
        })
    }
}

// TODO: add combinefunc, serialfunc, deserialfunc, and make this parallel safe
extension_sql!("\n\
    CREATE AGGREGATE toolkit_experimental.freq_agg(size double precision, value AnyElement)\n\
    (\n\
        sfunc = toolkit_experimental.freq_trans,\n\
        stype = internal,\n\
        finalfunc = toolkit_experimental.freq_final\n\
    );\n\
    ",
name = "freq_agg"); // TODO requires

#[pg_extern(immutable, parallel_safe, name="values", schema = "toolkit_experimental")]
pub fn freq_iter (
    agg: FrequencyAggregate<'_>,
    ty: AnyElement
) -> impl std::iter::Iterator<Item = (name!(value,AnyElement),name!(min_freq,f64),name!(max_freq,f64))> + '_ {
    unsafe {
        if ty.oid() != agg.type_oid {
            pgx::error!("mischatched types")
        }
        let counts = agg.counts.slice().iter().zip(agg.overcounts.slice().iter());
        agg.datums.clone().into_iter().zip(counts)
            .map_while(move |(value, (&count, &overcount))| {
                let total = agg.values_seen as f64;
                if count as f64 / total < agg.min_freq {
                    None
                } else {
                    let value = AnyElement::from_datum(value, false, agg.type_oid).unwrap();
                    let min_freq = (count - overcount) as f64 / total;
                    let max_freq = count as f64 / total;
                    Some((value, min_freq, max_freq))
                }
            })
    }
}

#[pg_extern(immutable, parallel_safe, schema = "toolkit_experimental")]
pub fn topn (
    agg: FrequencyAggregate<'_>,
    n: i32,
    ty: AnyElement
) -> impl std::iter::Iterator<Item = AnyElement> + '_ {
    unsafe {
        if ty.oid() != agg.type_oid {
            pgx::error!("mischatched types")
        }
        let iter = agg.datums.clone().into_iter().zip(agg.counts.slice().iter());
        iter.enumerate().map_while(move |(i, (value, &count))| {
                let total = agg.values_seen as f64;
                if i >= n as usize || count as f64 / total < agg.min_freq {
                    None
                } else {
                    AnyElement::from_datum(value, false, agg.type_oid)
                }
            })
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgx::*;
    use pgx_macros::pg_test;

    #[pg_test]
    fn test_freq_aggregate() {
        Spi::execute(|client| {
            // using the search path trick for this test to make it easier to stabilize later on
            let sp = client.select("SELECT format(' %s, toolkit_experimental',current_setting('search_path'))", None, None).first().get_one::<String>().unwrap();
            client.select(&format!("SET LOCAL search_path TO {}", sp), None, None);
            client.select("SET timescaledb_toolkit_acknowledge_auto_drop TO 'true'", None, None);

            client.select("CREATE TABLE test_val (data INTEGER)", None, None);
            client.select("CREATE TABLE test_varlena (data TEXT)", None, None);
            client.select("CREATE TABLE test_byref (data INTERVAL)", None, None);

            for i in (0..100).rev() {
                client.select(&format!("INSERT INTO test_val SELECT generate_series({}, 99, 1)", i), None, None);
                client.select(&format!("INSERT INTO test_varlena SELECT i::TEXT FROM generate_series({}, 99, 1) i", i), None, None);
                client.select(&format!("INSERT INTO test_byref SELECT (i::TEXT || ' seconds')::INTERVAL FROM generate_series({}, 99, 1) i", i), None, None);
            }

            let test = client.select("SELECT freq_agg(0.015, data)::TEXT FROM test_val", None, None)
                .first()
                .get_one::<String>().unwrap();
            // The way data gets inserted is fairly weird so the fact that this output is a bit odd isn't too concerning, 
            // randomizing the order of the table before inserting into the aggregate should give a better result
            let expected = "(version:1,type_oid:23,num_values:78,values_seen:5050,min_freq:0.015,counts:[100,99,98,97,96,95,94,93,92,91,90,89,88,87,86,85,84,84,83,82,81,81,80,80,79,78,78,77,76,76,76,75,75,75,75,75,75,75,75,75,75,75,75,75,75,75,75,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74],overcounts:[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,2,1,1,1,3,2,4,3,3,6,3,5,7,5,5,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73],datums:(type_oid:INT4,data_len:624,data:[99,0,0,0,0,0,0,0,98,0,0,0,0,0,0,0,97,0,0,0,0,0,0,0,96,0,0,0,0,0,0,0,95,0,0,0,0,0,0,0,94,0,0,0,0,0,0,0,93,0,0,0,0,0,0,0,92,0,0,0,0,0,0,0,91,0,0,0,0,0,0,0,90,0,0,0,0,0,0,0,89,0,0,0,0,0,0,0,88,0,0,0,0,0,0,0,87,0,0,0,0,0,0,0,86,0,0,0,0,0,0,0,85,0,0,0,0,0,0,0,84,0,0,0,0,0,0,0,81,0,0,0,0,0,0,0,82,0,0,0,0,0,0,0,83,0,0,0,0,0,0,0,80,0,0,0,0,0,0,0,77,0,0,0,0,0,0,0,78,0,0,0,0,0,0,0,75,0,0,0,0,0,0,0,76,0,0,0,0,0,0,0,79,0,0,0,0,0,0,0,73,0,0,0,0,0,0,0,74,0,0,0,0,0,0,0,71,0,0,0,0,0,0,0,69,0,0,0,0,0,0,0,70,0,0,0,0,0,0,0,72,0,0,0,0,0,0,0,53,0,0,0,0,0,0,0,54,0,0,0,0,0,0,0,55,0,0,0,0,0,0,0,56,0,0,0,0,0,0,0,57,0,0,0,0,0,0,0,58,0,0,0,0,0,0,0,59,0,0,0,0,0,0,0,60,0,0,0,0,0,0,0,61,0,0,0,0,0,0,0,62,0,0,0,0,0,0,0,63,0,0,0,0,0,0,0,64,0,0,0,0,0,0,0,65,0,0,0,0,0,0,0,66,0,0,0,0,0,0,0,67,0,0,0,0,0,0,0,68,0,0,0,0,0,0,0,22,0,0,0,0,0,0,0,23,0,0,0,0,0,0,0,24,0,0,0,0,0,0,0,25,0,0,0,0,0,0,0,26,0,0,0,0,0,0,0,27,0,0,0,0,0,0,0,28,0,0,0,0,0,0,0,29,0,0,0,0,0,0,0,30,0,0,0,0,0,0,0,31,0,0,0,0,0,0,0,32,0,0,0,0,0,0,0,33,0,0,0,0,0,0,0,34,0,0,0,0,0,0,0,35,0,0,0,0,0,0,0,36,0,0,0,0,0,0,0,37,0,0,0,0,0,0,0,38,0,0,0,0,0,0,0,39,0,0,0,0,0,0,0,40,0,0,0,0,0,0,0,41,0,0,0,0,0,0,0,42,0,0,0,0,0,0,0,43,0,0,0,0,0,0,0,44,0,0,0,0,0,0,0,45,0,0,0,0,0,0,0,46,0,0,0,0,0,0,0,47,0,0,0,0,0,0,0,48,0,0,0,0,0,0,0,49,0,0,0,0,0,0,0,50,0,0,0,0,0,0,0,51,0,0,0,0,0,0,0,21,0,0,0,0,0,0,0]))";
            assert_eq!(test, expected);

            let test = client.select("SELECT freq_agg(0.015, data)::TEXT FROM test_varlena", None, None)
                .first()
                .get_one::<String>().unwrap();
            let expected = "(version:1,type_oid:25,num_values:78,values_seen:5050,min_freq:0.015,counts:[100,99,98,97,96,95,94,93,92,91,90,89,88,87,86,85,84,84,83,82,81,81,80,80,79,78,78,77,76,76,76,75,75,75,75,75,75,75,75,75,75,75,75,75,75,75,75,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74],overcounts:[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,2,1,1,1,3,2,4,3,3,6,3,5,7,5,5,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73],datums:(type_oid:TEXT,data_len:624,data:[24,0,0,0,57,57,0,0,24,0,0,0,57,56,0,0,24,0,0,0,57,55,0,0,24,0,0,0,57,54,0,0,24,0,0,0,57,53,0,0,24,0,0,0,57,52,0,0,24,0,0,0,57,51,0,0,24,0,0,0,57,50,0,0,24,0,0,0,57,49,0,0,24,0,0,0,57,48,0,0,24,0,0,0,56,57,0,0,24,0,0,0,56,56,0,0,24,0,0,0,56,55,0,0,24,0,0,0,56,54,0,0,24,0,0,0,56,53,0,0,24,0,0,0,56,52,0,0,24,0,0,0,56,49,0,0,24,0,0,0,56,50,0,0,24,0,0,0,56,51,0,0,24,0,0,0,56,48,0,0,24,0,0,0,55,55,0,0,24,0,0,0,55,56,0,0,24,0,0,0,55,53,0,0,24,0,0,0,55,54,0,0,24,0,0,0,55,57,0,0,24,0,0,0,55,51,0,0,24,0,0,0,55,52,0,0,24,0,0,0,55,49,0,0,24,0,0,0,54,57,0,0,24,0,0,0,55,48,0,0,24,0,0,0,55,50,0,0,24,0,0,0,53,51,0,0,24,0,0,0,53,52,0,0,24,0,0,0,53,53,0,0,24,0,0,0,53,54,0,0,24,0,0,0,53,55,0,0,24,0,0,0,53,56,0,0,24,0,0,0,53,57,0,0,24,0,0,0,54,48,0,0,24,0,0,0,54,49,0,0,24,0,0,0,54,50,0,0,24,0,0,0,54,51,0,0,24,0,0,0,54,52,0,0,24,0,0,0,54,53,0,0,24,0,0,0,54,54,0,0,24,0,0,0,54,55,0,0,24,0,0,0,54,56,0,0,24,0,0,0,50,50,0,0,24,0,0,0,50,51,0,0,24,0,0,0,50,52,0,0,24,0,0,0,50,53,0,0,24,0,0,0,50,54,0,0,24,0,0,0,50,55,0,0,24,0,0,0,50,56,0,0,24,0,0,0,50,57,0,0,24,0,0,0,51,48,0,0,24,0,0,0,51,49,0,0,24,0,0,0,51,50,0,0,24,0,0,0,51,51,0,0,24,0,0,0,51,52,0,0,24,0,0,0,51,53,0,0,24,0,0,0,51,54,0,0,24,0,0,0,51,55,0,0,24,0,0,0,51,56,0,0,24,0,0,0,51,57,0,0,24,0,0,0,52,48,0,0,24,0,0,0,52,49,0,0,24,0,0,0,52,50,0,0,24,0,0,0,52,51,0,0,24,0,0,0,52,52,0,0,24,0,0,0,52,53,0,0,24,0,0,0,52,54,0,0,24,0,0,0,52,55,0,0,24,0,0,0,52,56,0,0,24,0,0,0,52,57,0,0,24,0,0,0,53,48,0,0,24,0,0,0,53,49,0,0,24,0,0,0,50,49,0,0]))";
            assert_eq!(test, expected);

            let test = client.select("SELECT freq_agg(0.015, data)::TEXT FROM test_byref", None, None)
                .first()
                .get_one::<String>().unwrap();
            let expected = "(version:1,type_oid:1186,num_values:78,values_seen:5050,min_freq:0.015,counts:[100,99,98,97,96,95,94,93,92,91,90,89,88,87,86,85,84,84,83,82,81,81,80,80,79,78,78,77,76,76,76,75,75,75,75,75,75,75,75,75,75,75,75,75,75,75,75,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74],overcounts:[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,2,1,1,1,3,2,4,3,3,6,3,5,7,5,5,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,74,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73,73],datums:(type_oid:INTERVAL,data_len:1248,data:[192,158,230,5,0,0,0,0,0,0,0,0,0,0,0,0,128,92,215,5,0,0,0,0,0,0,0,0,0,0,0,0,64,26,200,5,0,0,0,0,0,0,0,0,0,0,0,0,0,216,184,5,0,0,0,0,0,0,0,0,0,0,0,0,192,149,169,5,0,0,0,0,0,0,0,0,0,0,0,0,128,83,154,5,0,0,0,0,0,0,0,0,0,0,0,0,64,17,139,5,0,0,0,0,0,0,0,0,0,0,0,0,0,207,123,5,0,0,0,0,0,0,0,0,0,0,0,0,192,140,108,5,0,0,0,0,0,0,0,0,0,0,0,0,128,74,93,5,0,0,0,0,0,0,0,0,0,0,0,0,64,8,78,5,0,0,0,0,0,0,0,0,0,0,0,0,0,198,62,5,0,0,0,0,0,0,0,0,0,0,0,0,192,131,47,5,0,0,0,0,0,0,0,0,0,0,0,0,128,65,32,5,0,0,0,0,0,0,0,0,0,0,0,0,64,255,16,5,0,0,0,0,0,0,0,0,0,0,0,0,0,189,1,5,0,0,0,0,0,0,0,0,0,0,0,0,64,246,211,4,0,0,0,0,0,0,0,0,0,0,0,0,128,56,227,4,0,0,0,0,0,0,0,0,0,0,0,0,192,122,242,4,0,0,0,0,0,0,0,0,0,0,0,0,0,180,196,4,0,0,0,0,0,0,0,0,0,0,0,0,64,237,150,4,0,0,0,0,0,0,0,0,0,0,0,0,128,47,166,4,0,0,0,0,0,0,0,0,0,0,0,0,192,104,120,4,0,0,0,0,0,0,0,0,0,0,0,0,0,171,135,4,0,0,0,0,0,0,0,0,0,0,0,0,192,113,181,4,0,0,0,0,0,0,0,0,0,0,0,0,64,228,89,4,0,0,0,0,0,0,0,0,0,0,0,0,128,38,105,4,0,0,0,0,0,0,0,0,0,0,0,0,192,95,59,4,0,0,0,0,0,0,0,0,0,0,0,0,64,219,28,4,0,0,0,0,0,0,0,0,0,0,0,0,128,29,44,4,0,0,0,0,0,0,0,0,0,0,0,0,0,162,74,4,0,0,0,0,0,0,0,0,0,0,0,0,64,183,40,3,0,0,0,0,0,0,0,0,0,0,0,0,128,249,55,3,0,0,0,0,0,0,0,0,0,0,0,0,192,59,71,3,0,0,0,0,0,0,0,0,0,0,0,0,0,126,86,3,0,0,0,0,0,0,0,0,0,0,0,0,64,192,101,3,0,0,0,0,0,0,0,0,0,0,0,0,128,2,117,3,0,0,0,0,0,0,0,0,0,0,0,0,192,68,132,3,0,0,0,0,0,0,0,0,0,0,0,0,0,135,147,3,0,0,0,0,0,0,0,0,0,0,0,0,64,201,162,3,0,0,0,0,0,0,0,0,0,0,0,0,128,11,178,3,0,0,0,0,0,0,0,0,0,0,0,0,192,77,193,3,0,0,0,0,0,0,0,0,0,0,0,0,0,144,208,3,0,0,0,0,0,0,0,0,0,0,0,0,64,210,223,3,0,0,0,0,0,0,0,0,0,0,0,0,128,20,239,3,0,0,0,0,0,0,0,0,0,0,0,0,192,86,254,3,0,0,0,0,0,0,0,0,0,0,0,0,0,153,13,4,0,0,0,0,0,0,0,0,0,0,0,0,128,177,79,1,0,0,0,0,0,0,0,0,0,0,0,0,192,243,94,1,0,0,0,0,0,0,0,0,0,0,0,0,0,54,110,1,0,0,0,0,0,0,0,0,0,0,0,0,64,120,125,1,0,0,0,0,0,0,0,0,0,0,0,0,128,186,140,1,0,0,0,0,0,0,0,0,0,0,0,0,192,252,155,1,0,0,0,0,0,0,0,0,0,0,0,0,0,63,171,1,0,0,0,0,0,0,0,0,0,0,0,0,64,129,186,1,0,0,0,0,0,0,0,0,0,0,0,0,128,195,201,1,0,0,0,0,0,0,0,0,0,0,0,0,192,5,217,1,0,0,0,0,0,0,0,0,0,0,0,0,0,72,232,1,0,0,0,0,0,0,0,0,0,0,0,0,64,138,247,1,0,0,0,0,0,0,0,0,0,0,0,0,128,204,6,2,0,0,0,0,0,0,0,0,0,0,0,0,192,14,22,2,0,0,0,0,0,0,0,0,0,0,0,0,0,81,37,2,0,0,0,0,0,0,0,0,0,0,0,0,64,147,52,2,0,0,0,0,0,0,0,0,0,0,0,0,128,213,67,2,0,0,0,0,0,0,0,0,0,0,0,0,192,23,83,2,0,0,0,0,0,0,0,0,0,0,0,0,0,90,98,2,0,0,0,0,0,0,0,0,0,0,0,0,64,156,113,2,0,0,0,0,0,0,0,0,0,0,0,0,128,222,128,2,0,0,0,0,0,0,0,0,0,0,0,0,192,32,144,2,0,0,0,0,0,0,0,0,0,0,0,0,0,99,159,2,0,0,0,0,0,0,0,0,0,0,0,0,64,165,174,2,0,0,0,0,0,0,0,0,0,0,0,0,128,231,189,2,0,0,0,0,0,0,0,0,0,0,0,0,192,41,205,2,0,0,0,0,0,0,0,0,0,0,0,0,0,108,220,2,0,0,0,0,0,0,0,0,0,0,0,0,64,174,235,2,0,0,0,0,0,0,0,0,0,0,0,0,128,240,250,2,0,0,0,0,0,0,0,0,0,0,0,0,192,50,10,3,0,0,0,0,0,0,0,0,0,0,0,0,64,111,64,1,0,0,0,0,0,0,0,0,0,0,0,0]))";
            assert_eq!(test, expected);
        });
    }
}
