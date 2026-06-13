//! Standalone REAPER extension exposing only the Pot preset browser engine.
//!
//! Registers the `HB_Pot_*` ReaScript API and warms up the preset databases, without
//! pulling in ReaLearn. Build this instead of the full Helgobox plugin when you only
//! need the pot engine — e.g. as the backend for the Lua/ReaImGui browser. The artifact
//! is a fraction of `libhelgobox.so` because it doesn't carry ReaLearn and its
//! dependency tree (tonic, axum, the Flutter app, Stream Deck, egui, …).

use pot_api::register_pot_api;
use reaper_high::{PluginInfo, Reaper};
use reaper_low::{PluginContext, Swell};
use reaper_macros::reaper_extension_plugin;
use reaper_medium::RegistrationObject;
use std::error::Error;

#[reaper_extension_plugin]
fn plugin_main(context: PluginContext) -> Result<(), Box<dyn Error>> {
    // Set up SWELL globally. Required because parts of the pot stack (e.g. swell-ui's
    // dark-mode detection, used by HB_Pot_DarkModeEnabled) call Swell::get(), which
    // panics if SWELL was never made available — and a panic across the extension's C
    // ABI aborts REAPER. The full Helgobox plugin does this too.
    let _ = Swell::make_available_globally(Swell::load(context));
    // Set up reaper-high globally (gives us Reaper::get(), the medium session, etc.).
    Reaper::setup_with_defaults(context, plugin_info())
        .map_err(|_| "couldn't set up REAPER for the Pot extension".to_string())?;
    // Register the HB_Pot_* API functions (API_*, APIvararg_*, APIdef_* for each).
    {
        let mut session = Reaper::get().medium_session();
        let mut op = |reg: RegistrationObject| -> Result<(), Box<dyn Error>> {
            unsafe { session.plugin_register_add(reg) }
                .map_err(|e| -> Box<dyn Error> { e.to_string().into() })?;
            Ok(())
        };
        register_pot_api(&mut op)?;
    }
    // Warm up the preset databases in the background so the first browser open is instant.
    pot::spawn_in_pot_worker(async {
        pot::pot_db().refresh();
        Ok(())
    });
    Ok(())
}

fn plugin_info() -> PluginInfo {
    PluginInfo {
        plugin_name: "Helgobox Pot".to_string(),
        plugin_version: env!("CARGO_PKG_VERSION").to_string(),
        plugin_version_long: env!("CARGO_PKG_VERSION").to_string(),
        support_email_address: "info@helgoboss.org".to_string(),
        update_url: "https://www.helgoboss.org/projects/helgobox".to_string(),
    }
}
