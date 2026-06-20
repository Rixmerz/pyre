// Prevents an extra console window on Windows in release. Linux-first spike.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    pyre_gui_lib::run();
}
