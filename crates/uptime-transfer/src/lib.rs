//! Offline transfers preserve IDs and every application table; no workers run here.

pub mod postgres;
pub mod schema;
pub mod snapshot;
pub mod turso;

use schema::Table;

pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) fn columns(table: &Table) -> String {
    table
        .columns
        .iter()
        .map(|(name, _)| quote(name))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn order(table: &Table) -> String {
    table
        .keys
        .iter()
        .map(|name| quote(name))
        .collect::<Vec<_>>()
        .join(", ")
}
