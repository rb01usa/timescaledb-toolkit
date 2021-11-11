
use aggregate_builder::aggregate;

use pgx::*;

use crate::palloc::Inner;

// just about the simplest aggregate `arbitrary()` returns an arbitrary element
// from the input set. It's mainly here for demonstration purposes, but it can
// have some use due with GROUP BY
#[aggregate] impl toolkit_experimental::arbitrary {
    type State = String;

    fn transition(
        state: Option<Inner<State>>,
        #[sql_type("text")] value: String,
    ) -> Option<Inner<State>> {
        match state {
            Some(value) => Some(value),
            None => Some(value.into()),
        }
    }

    fn finally(state: Option<Inner<State>>) -> Option<String> {
        state.as_deref().cloned()
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgx::*;
    use pgx_macros::pg_test;

    #[pg_test]
    fn test_arbitrary_in_experimental_and_returns_first() {
        Spi::execute(|client| {
            let output = client.select(
                "SELECT toolkit_experimental.arbitrary(val) \
                FROM (VALUES ('foo'), ('bar'), ('baz')) as v(val)",
                None,
                None,
            ).first()
                .get_one::<String>();
            assert_eq!(output.as_deref(), Some("foo"));
        })
    }
}