// Evita la consola extra en Windows en modo release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    cabina_lib::run()
}
