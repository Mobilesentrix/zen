//! Integration tests against real MySQL + PostgreSQL (ms-test/docker-compose.yml).
//! Run: see MS-BUILD.md ("Integration tests"). They need MS_TEST_MYSQL_URL / MS_TEST_PG_URL.

mod basics;
mod limits;
mod loop_mode;
mod result_handling;
mod round_trips;
mod support;
mod writes;
