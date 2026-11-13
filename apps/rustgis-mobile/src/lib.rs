//! RustGIS on Android: the phase 0 feasibility spike (#1902).
//!
//! The desktop front end (`rustgis-ui-egui`) in a `NativeActivity`, to find out what builds, what
//! runs and what breaks on a phone before any touch interface is designed. See `README.md` for
//! how to build and install it.
//!
//! Not here yet (phase 1): a platform trait for location, camera and file picking; preferences
//! and the keychain; autosave; a touch-first front end.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[cfg(target_os = "android")]
mod android {
    use eframe::egui;
    use eframe::wgpu;
    use rustgis_engine::data::dirs;
    use rustgis_ui_egui::RustGisApp;
    use winit::platform::android::activity::AndroidApp;

    /// The app: the desktop front end, drawn below the status bar and above the navigation bar.
    struct App(RustGisApp);

    impl eframe::App for App {
        fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
            // The GPU map backend draws inside egui's wgpu pass; hand over the render state.
            self.0.set_render_state(frame.wgpu_render_state().cloned());
            self.0.logic(ctx);
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            self.0.ui(ui);
        }

        fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
            self.0.drop_stale_clicks(raw_input);
        }
    }

    /// Points the app's folders (`rustgis_data::dirs`) at the app's own storage: Android has no
    /// `HOME`, so without this there are no settings, no tile cache and no recovery folder.
    ///
    /// Run first in [`android_main`], before this library starts a thread of its own.
    #[allow(unsafe_code)]
    fn set_folders(app: &AndroidApp) {
        let Some(data) = app.internal_data_path() else {
            log::warn!("no internal data folder: settings and the cache are off");
            return;
        };
        for (name, sub) in [(dirs::CONFIG_ENV, "config"), (dirs::APP_DATA_ENV, "data"), (dirs::CACHE_ENV, "cache")] {
            // SAFETY: called once at the top of `android_main`, before this library reads the
            // environment or starts a thread. The Java runtime's threads are alive, which is the
            // spike's known debt: phase 1 passes the folders in instead of through the environment.
            unsafe { std::env::set_var(name, data.join(sub)) };
        }
    }

    /// Opens the files in the app's external files folder
    /// (`/sdcard/Android/data/org.rustgis.mobile/files`, where `adb push` can put them), in name
    /// order: the spike's stand-in for a file picker. What doesn't open is said in the window.
    fn open_pushed(android: &AndroidApp, app: &mut RustGisApp) {
        let Some(dir) = android.external_data_path() else { return };
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let mut files: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
        files.sort();
        for file in files {
            let path = file.display().to_string();
            match app.run(rustgis_engine::project::open_command(&path), serde_json::json!({"path": path})) {
                Ok(_) => log::info!("opened {path}"),
                Err(e) => {
                    log::warn!("{path}: {e}");
                    app.session.notify(format!("{path}: {e}"), true);
                }
            }
        }
    }

    /// Android's entry point for a `NativeActivity` (`android-activity` calls it on its own thread).
    #[allow(unsafe_code)]
    #[unsafe(no_mangle)]
    fn android_main(android: AndroidApp) {
        android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("rustgis"));
        set_folders(&android);
        let mut app = RustGisApp::with_sample();
        // As on the desktop: commands run on the window's thread must not hold it.
        app.session.in_place_wait = false;
        app.session.on_window_thread = true;
        app.reads_off_frame = true;
        if let Err(e) = app.start(&rustgis_ui_egui::Start { blank: Some(false), ..Default::default() }) {
            log::warn!("start-up: {e}");
        }
        open_pushed(&android, &mut app);
        app.session.mark_saved();
        let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
        if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
            // Vulkan where the phone has it, OpenGL ES otherwise (older phones, the emulator).
            setup.instance_descriptor.backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
        }
        let options = eframe::NativeOptions { android_app: Some(android), wgpu_options, ..Default::default() };
        if let Err(e) = eframe::run_native("RustGIS", options, Box::new(move |_cc| Ok(Box::new(App(app))))) {
            log::error!("the app stopped: {e}");
        }
        // The activity is gone (Back, or the system destroyed it). winit's event loop can't be
        // made twice in a process, so a process left alive would answer the next launch with a
        // blank window: end it, and the next launch starts afresh. Phase 1 keeps the session
        // across this instead (`onSaveInstanceState`).
        std::process::exit(0);
    }
}
