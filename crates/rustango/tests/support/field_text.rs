//! A form shows a field's label and help text from the model's schema,
//! so the console and the admin read one copy.
#![allow(dead_code)]

/// Assert `html` labels `field` of `schema` and renders its help text.
#[track_caller]
pub fn assert_shows(html: &str, schema: &rustango::core::ModelSchema, field: &str, input_id: &str) {
    let f = schema
        .field(field)
        .unwrap_or_else(|| panic!("{}.{field}", schema.table));
    let label = format!(r#"<label for="{input_id}">{}</label>"#, f.display_label());
    assert!(html.contains(&label), "no `{label}` in:\n{html}");
    let help = rustango::template_extensions::help_html(f.help_text.expect("help_text"));
    assert!(
        html.contains(&help),
        "no help text of `{field}` in:\n{html}"
    );
}
