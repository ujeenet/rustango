//! Admin-side re-export of the public form parsers, which live in
//! [`crate::forms`]. The admin's CRUD handlers reach them through
//! `super::forms::*`.
//!
//! The `admin` feature implies `forms`, so this is always available
//! when the admin is on.

pub(crate) use crate::forms::{
    collect_insert_values, collect_values, parse_form_value, parse_pk_string, FormError,
};
