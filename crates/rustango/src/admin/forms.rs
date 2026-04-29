//! Admin-side re-export of the public form parsers (slice 8.4A).
//!
//! Pre-v0.8 this file held the parsers as `pub(crate)` admin-internal
//! helpers; v0.8 promoted them to `rustango::forms`. The admin's
//! existing CRUD handlers reach them through `super::forms::*`, so
//! this re-export keeps every call site working without an edit.
//!
//! The `admin` feature implies `forms`, so this re-export is always
//! available when admin is on.

pub(crate) use crate::forms::{collect_values, parse_form_value, parse_pk_string, FormError};
