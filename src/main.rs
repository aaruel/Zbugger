#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod profiling;

mod app;
mod build;
mod debug;
mod document;
mod editor;
mod find;
mod image_view;
mod navigation;
mod output_panel;
mod preferences;
mod project;
mod project_view;
mod search;
mod search_panel;
mod symbols;
mod syntax;
mod toolbar;
mod unreal;

/// mimalloc serves Rust's allocations: much faster than the Windows heap for
/// the many small, multi-threaded allocations of indexing, and unaffected by
/// the Windows debug heap when running under a debugger.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Route tree-sitter's C allocations (syntax trees, query cursors) through
/// mimalloc too.
fn use_mimalloc_for_tree_sitter() {
    let allocator = tree_sitter::Allocator {
        malloc: libmimalloc_sys::mi_malloc,
        calloc: libmimalloc_sys::mi_calloc,
        realloc: libmimalloc_sys::mi_realloc,
        free: libmimalloc_sys::mi_free,
    };
    // SAFETY: called once at startup, before any tree-sitter object exists,
    // so nothing allocated with the old allocator is freed with the new one.
    unsafe { tree_sitter::set_allocator(Some(allocator)) };
}

fn main() -> eframe::Result<()> {
    use_mimalloc_for_tree_sitter();
    // Does nothing unless built with `--features tracy`.
    tracy_client::Client::start();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Zbugger")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([400.0, 300.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Zbugger",
        options,
        Box::new(|cc| Ok(Box::new(app::ZbuggerApp::new(cc)))),
    )
}
