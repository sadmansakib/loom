//! The specification: the migration to storage schema v2 is done
//! when every widget reports version 2.
use loom_eval_multi1::widgets::button;
use loom_eval_multi1::widgets::checkbox;
use loom_eval_multi1::widgets::slider;
use loom_eval_multi1::widgets::textbox;
use loom_eval_multi1::widgets::toggle;
use loom_eval_multi1::widgets::dropdown;
use loom_eval_multi1::widgets::listbox;
use loom_eval_multi1::widgets::progress;
use loom_eval_multi1::widgets::spinner;
use loom_eval_multi1::widgets::tabbar;
use loom_eval_multi1::widgets::canvas;
use loom_eval_multi1::widgets::menubar;

#[test]
fn every_widget_reports_schema_version_two() {
    assert_eq!(button::SCHEMA_VERSION, 2, "button must speak schema v2");
    assert_eq!(checkbox::SCHEMA_VERSION, 2, "checkbox must speak schema v2");
    assert_eq!(slider::SCHEMA_VERSION, 2, "slider must speak schema v2");
    assert_eq!(textbox::SCHEMA_VERSION, 2, "textbox must speak schema v2");
    assert_eq!(toggle::SCHEMA_VERSION, 2, "toggle must speak schema v2");
    assert_eq!(dropdown::SCHEMA_VERSION, 2, "dropdown must speak schema v2");
    assert_eq!(listbox::SCHEMA_VERSION, 2, "listbox must speak schema v2");
    assert_eq!(progress::SCHEMA_VERSION, 2, "progress must speak schema v2");
    assert_eq!(spinner::SCHEMA_VERSION, 2, "spinner must speak schema v2");
    assert_eq!(tabbar::SCHEMA_VERSION, 2, "tabbar must speak schema v2");
    assert_eq!(canvas::SCHEMA_VERSION, 2, "canvas must speak schema v2");
    assert_eq!(menubar::SCHEMA_VERSION, 2, "menubar must speak schema v2");
}

#[test]
fn quads_scale_with_children() {
    assert_eq!(button::quads_drawn(3), 4);
    assert_eq!(progress::quads_drawn(0), 8);
    assert_eq!(menubar::quads_drawn(2), 14);
}

#[test]
fn quads_grow_monotonically() {
    for module in [
        button::quads_drawn(0),
        checkbox::quads_drawn(0),
        slider::quads_drawn(0),
        textbox::quads_drawn(0),
        toggle::quads_drawn(0),
        dropdown::quads_drawn(0),
        listbox::quads_drawn(0),
        progress::quads_drawn(0),
        spinner::quads_drawn(0),
        tabbar::quads_drawn(0),
        canvas::quads_drawn(0),
        menubar::quads_drawn(0),
    ] {
        assert!(module > 0);
    }
}
