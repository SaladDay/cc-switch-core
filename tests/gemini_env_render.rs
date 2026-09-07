use cc_switch_core::gemini::render_literal_env_assignments;

#[test]
fn sorts_names_and_keeps_literal_text_and_duplicate_order() {
    assert_eq!(render_literal_env_assignments([]), "");
    assert_eq!(render_literal_env_assignments([("A", "")]), "A=");
    assert_eq!(
        render_literal_env_assignments([
            ("变量", "first"),
            ("Z", "\"quoted\""),
            ("A", "  $TOKEN='x=y'  "),
            ("变量", "second"),
        ]),
        "A=  $TOKEN='x=y'  \nZ=\"quoted\"\n变量=first\n变量=second",
    );
}

#[test]
fn formatting_does_not_silently_validate_or_escape_native_data() {
    assert_eq!(
        render_literal_env_assignments([("bad=name", "one\ntwo\r\0"), ("", "# literal")]),
        "=# literal\nbad=name=one\ntwo\r\0",
    );
}
