#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Desktop-Hülle für das ERLC CAD.
//!
//! Das Hauptfenster lädt die Live-Seite; Login, Passkeys und Daten bleiben beim
//! Server. Dazu gibt es kleine Widget-Fenster (Uhr, Dienst, Einsätze …), die du
//! über das Tray-Icon einzeln ein- und ausschaltest und frei auf den Bildschirm
//! legst. Jedes lädt `/overlay/<widget>` derselben Seite.
//!
//! Die Seiten bekommen keinen Zugriff auf die App (keine Capabilities) – alles
//! Fensterverhalten läuft hier in Rust:
//!
//! - Strg+Alt+C        alle Fenster aus-/einblenden
//! - Strg+Alt+X        Spielmodus: Mausklicks gehen durch die Fenster ins Spiel
//! - Strg+Alt+E        Widgets bearbeiten: Rahmen zum Verschieben und Skalieren
//! - Strg+Alt+↑ / ↓    Fenster durchsichtiger / deckender
//!
//! Es sind eigene Fenster neben Roblox: kein Eingriff ins Spiel.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use tauri::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::PageLoadEvent,
    AppHandle, Manager, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, Wry, WindowEvent,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_updater::UpdaterExt;
use tauri_plugin_window_state::StateFlags;

const BASE_HOST: &str = "erlc-cad-xi.vercel.app";
const BASE_URL: &str = "https://erlc-cad-xi.vercel.app";

struct WidgetDef {
    id: &'static str,
    title: &'static str,
    width: f64,
    height: f64,
}

/// Die wählbaren Widgets – dieselben Kennungen wie `WIDGET_IDS` im CAD.
static WIDGETS: [WidgetDef; 6] = [
    WidgetDef { id: "clock", title: "Uhr", width: 230.0, height: 130.0 },
    WidgetDef { id: "duty", title: "Dienststatus", width: 270.0, height: 170.0 },
    WidgetDef { id: "calls", title: "Offene Einsätze", width: 210.0, height: 90.0 },
    WidgetDef { id: "players", title: "Spieler online", width: 210.0, height: 90.0 },
    WidgetDef { id: "bolos", title: "Aktive Fahndungen", width: 210.0, height: 90.0 },
    WidgetDef { id: "notes", title: "Notizzettel", width: 240.0, height: 170.0 },
];

struct Overlay {
    click_through: AtomicBool,
    edit: AtomicBool,
    /// Wird beim Beenden gesetzt: die dabei schließenden Fenster sollen die
    /// gemerkte Widget-Auswahl nicht überschreiben.
    quitting: AtomicBool,
    /// Deckkraft in Prozent (30–100).
    opacity: AtomicU32,
}

/// Menüeinträge, deren Haken wir nachführen (Hotkey, X im Rahmen …).
struct TrayItems {
    widgets: Vec<(&'static str, CheckMenuItem<Wry>)>,
    edit: CheckMenuItem<Wry>,
    click: CheckMenuItem<Wry>,
}

fn shortcut(code: Code) -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), code)
}

fn widget_label(id: &str) -> String {
    format!("w-{id}")
}

fn is_widget(window: &WebviewWindow) -> bool {
    window.label().starts_with("w-")
}

fn all_windows(app: &AppHandle) -> Vec<WebviewWindow> {
    app.webview_windows().into_values().collect()
}

// ----------------------------------------------------------- gemerkte Auswahl

fn saved_file(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|dir| dir.join("widgets.txt"))
}

/// Merkt, welche Widgets offen sind. `except`: ein Fenster, das gerade schließt.
fn save_open(app: &AppHandle, except: Option<&str>) {
    if app.state::<Overlay>().quitting.load(Ordering::Relaxed) {
        return;
    }
    let open: Vec<&str> = WIDGETS
        .iter()
        .filter(|def| {
            let label = widget_label(def.id);
            Some(label.as_str()) != except && app.get_webview_window(&label).is_some()
        })
        .map(|def| def.id)
        .collect();
    if let Some(path) = saved_file(app) {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(path, open.join("\n"));
    }
}

fn load_open(app: &AppHandle) -> Vec<&'static str> {
    let Some(text) = saved_file(app).and_then(|path| fs::read_to_string(path).ok()) else {
        return Vec::new();
    };
    WIDGETS
        .iter()
        .filter(|def| text.lines().any(|line| line.trim() == def.id))
        .map(|def| def.id)
        .collect()
}

fn set_widget_check(app: &AppHandle, id: &str, checked: bool) {
    if let Some(items) = app.try_state::<TrayItems>() {
        if let Some((_, item)) = items.widgets.iter().find(|(widget, _)| *widget == id) {
            let _ = item.set_checked(checked);
        }
    }
}

// ------------------------------------------------------------ Fensterverhalten

/// Mausklicks durchlassen? Nicht im Bearbeiten-Modus – da sollst du greifen können.
fn apply_click_through(app: &AppHandle) {
    let state = app.state::<Overlay>();
    let on = state.click_through.load(Ordering::Relaxed) && !state.edit.load(Ordering::Relaxed);
    for window in all_windows(app) {
        let _ = window.set_ignore_cursor_events(on);
    }
}

fn set_click_through(app: &AppHandle, on: bool) {
    let state = app.state::<Overlay>();
    state.click_through.store(on, Ordering::Relaxed);
    if on {
        // Spielmodus und Bearbeiten schließen sich aus.
        state.edit.store(false, Ordering::Relaxed);
    }
    apply_edit_mode(app);
    apply_click_through(app);
    sync_checks(app);
}

fn toggle_click_through(app: &AppHandle) {
    let on = !app.state::<Overlay>().click_through.load(Ordering::Relaxed);
    set_click_through(app, on);
}

fn apply_edit_mode(app: &AppHandle) {
    let edit = app.state::<Overlay>().edit.load(Ordering::Relaxed);
    for window in all_windows(app).iter().filter(|window| is_widget(window)) {
        let _ = window.set_decorations(edit);
    }
}

fn toggle_edit(app: &AppHandle) {
    let state = app.state::<Overlay>();
    let on = !state.edit.load(Ordering::Relaxed);
    state.edit.store(on, Ordering::Relaxed);
    if on {
        state.click_through.store(false, Ordering::Relaxed);
    }
    apply_edit_mode(app);
    apply_click_through(app);
    sync_checks(app);
}

fn sync_checks(app: &AppHandle) {
    let state = app.state::<Overlay>();
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.edit.set_checked(state.edit.load(Ordering::Relaxed));
        let _ = items.click.set_checked(state.click_through.load(Ordering::Relaxed));
    }
}

/// Alles ausblenden, wenn irgendetwas sichtbar ist – sonst alles einblenden.
fn toggle_visible(app: &AppHandle) {
    let windows = all_windows(app);
    let any_visible = windows.iter().any(|window| window.is_visible().unwrap_or(false));
    for window in windows {
        if any_visible {
            let _ = window.hide();
        } else {
            let _ = window.show();
        }
    }
}

fn apply_opacity(app: &AppHandle) {
    let pct = app.state::<Overlay>().opacity.load(Ordering::Relaxed);
    let script = format!("document.documentElement.style.opacity = '{}';", pct as f32 / 100.0);
    for window in all_windows(app) {
        let _ = window.eval(&script);
    }
}

fn change_opacity(app: &AppHandle, delta: i32) {
    let state = app.state::<Overlay>();
    let current = state.opacity.load(Ordering::Relaxed) as i32;
    state
        .opacity
        .store((current + delta).clamp(30, 100) as u32, Ordering::Relaxed);
    apply_opacity(app);
}

// -------------------------------------------------------------------- Widgets

/// Öffnet das Widget als eigenes Fenster. Fenster entstehen nicht im
/// Menü-Handler selbst (unter Windows kann das hängen), sondern in einem Thread.
fn open_widget(app: &AppHandle, def: &'static WidgetDef) {
    let label = widget_label(def.id);
    if app.get_webview_window(&label).is_some() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let Ok(url) = format!("{BASE_URL}/overlay/{}", def.id).parse::<Url>() else {
            return;
        };
        let state = app.state::<Overlay>();
        let edit = state.edit.load(Ordering::Relaxed);
        // Neue Widgets leicht versetzt, damit sie sich nicht genau überdecken.
        let offset = 40.0 + 28.0 * all_windows(&app).len() as f64;

        let built = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(url))
            .title(def.title)
            .inner_size(def.width, def.height)
            .min_inner_size(120.0, 56.0)
            .position(offset, offset)
            .decorations(edit)
            .transparent(true)
            .shadow(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(true)
            .build();

        match built {
            Ok(_) => {
                set_widget_check(&app, def.id, true);
                apply_click_through(&app);
                save_open(&app, None);
            }
            Err(error) => {
                eprintln!("Widget {} konnte nicht geöffnet werden: {error}", def.id);
                set_widget_check(&app, def.id, false);
            }
        }
    });
}

fn toggle_widget(app: &AppHandle, def: &'static WidgetDef) {
    match app.get_webview_window(&widget_label(def.id)) {
        Some(window) => {
            let _ = window.close();
            // Die gemerkte Auswahl führt das `Destroyed`-Ereignis nach.
        }
        None => open_widget(app, def),
    }
}

// ------------------------------------------------------------------- Updates

fn info_dialog(app: &AppHandle, kind: MessageDialogKind, message: String) {
    app.dialog()
        .message(message)
        .title("ERLC CAD – Update")
        .kind(kind)
        .show(|_| {});
}

/// Sucht nach einer neuen Version (GitHub Releases, Signatur wird geprüft).
/// Beim automatischen Lauf bleibt es still, wenn nichts da ist oder die
/// Suche scheitert (kein Netz, noch kein Release); manuell kommt eine Antwort.
async fn check_for_update(app: AppHandle, manual: bool) {
    let updater = match app.updater() {
        Ok(updater) => updater,
        Err(error) => {
            eprintln!("Updater nicht verfügbar: {error}");
            return;
        }
    };

    match updater.check().await {
        Ok(Some(update)) => {
            let notes = update.body.clone().unwrap_or_default();
            let message = format!(
                "Version {} ist verfügbar (installiert: {}).\n\n{}\n\nJetzt installieren? Die App startet danach neu.",
                update.version,
                env!("CARGO_PKG_VERSION"),
                notes
            );
            let handle = app.clone();
            app.dialog()
                .message(message)
                .title("ERLC CAD – Update")
                .kind(MessageDialogKind::Info)
                .buttons(MessageDialogButtons::OkCancelCustom("Installieren".into(), "Später".into()))
                .show(move |install| {
                    if !install {
                        return;
                    }
                    tauri::async_runtime::spawn(async move {
                        // Der Installer beendet die App: die dabei schließenden
                        // Widget-Fenster sollen die gemerkte Auswahl nicht löschen.
                        handle.state::<Overlay>().quitting.store(true, Ordering::Relaxed);
                        if let Err(error) = update.download_and_install(|_, _| {}, || {}).await {
                            handle.state::<Overlay>().quitting.store(false, Ordering::Relaxed);
                            info_dialog(&handle, MessageDialogKind::Error, format!("Das Update konnte nicht installiert werden:\n{error}"));
                        }
                    });
                });
        }
        Ok(None) => {
            if manual {
                info_dialog(
                    &app,
                    MessageDialogKind::Info,
                    format!("Du hast die neueste Version ({}).", env!("CARGO_PKG_VERSION")),
                );
            }
        }
        Err(error) => {
            eprintln!("Update-Suche fehlgeschlagen: {error}");
            if manual {
                info_dialog(&app, MessageDialogKind::Warning, format!("Die Update-Suche ist fehlgeschlagen:\n{error}"));
            }
        }
    }
}

/// Automatische Prüfung: kurz nach dem Start, danach alle sechs Stunden.
fn start_update_checks(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(15));
        loop {
            tauri::async_runtime::block_on(check_for_update(app.clone(), false));
            std::thread::sleep(std::time::Duration::from_secs(6 * 60 * 60));
        }
    });
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Overlay {
            click_through: AtomicBool::new(false),
            edit: AtomicBool::new(false),
            quitting: AtomicBool::new(false),
            opacity: AtomicU32::new(100),
        })
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::POSITION | StateFlags::SIZE)
                .build(),
        )
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, pressed, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    if pressed == &shortcut(Code::KeyC) {
                        toggle_visible(app);
                    } else if pressed == &shortcut(Code::KeyX) {
                        toggle_click_through(app);
                    } else if pressed == &shortcut(Code::KeyE) {
                        toggle_edit(app);
                    } else if pressed == &shortcut(Code::ArrowUp) {
                        change_opacity(app, -10);
                    } else if pressed == &shortcut(Code::ArrowDown) {
                        change_opacity(app, 10);
                    }
                })
                .build(),
        )
        .plugin(
            // Verbindung zur Seite ohne IPC: Die Seite erfährt über eine Kennung,
            // dass sie in der App läuft, und „lagert“ ein Widget aus, indem sie
            // `/overlay/<widget>` aufruft. Das Hauptfenster bricht diese
            // Navigation ab und öffnet stattdessen das Widget-Fenster.
            tauri::plugin::Builder::<Wry>::new("popout")
                .js_init_script("window.__ERLC_DESKTOP__ = true;".to_string())
                .on_navigation(|webview, url| {
                    if url.host_str() != Some(BASE_HOST) {
                        return true;
                    }
                    let Some(id) = url.path().strip_prefix("/overlay/") else {
                        return true;
                    };

                    // Hauptfenster: ein Widget soll ausgelagert werden.
                    if webview.label() == "main" {
                        if let Some(def) = WIDGETS.iter().find(|def| def.id == id.trim_end_matches('/')) {
                            open_widget(webview.app_handle(), def);
                        }
                        return false;
                    }

                    // Widget-Fenster: der Schließen-Knopf der Seite ruft
                    // `/overlay/<widget>?close=1` auf. Das erste Laden des
                    // Fensters hat keinen `close`-Parameter und geht durch.
                    if webview.label().starts_with("w-") && url.query_pairs().any(|(key, _)| key == "close") {
                        let app = webview.app_handle().clone();
                        let label = webview.label().to_string();
                        // Nicht im Navigations-Handler selbst schließen.
                        std::thread::spawn(move || {
                            if let Some(window) = app.get_webview_window(&label) {
                                let _ = window.close();
                            }
                        });
                        return false;
                    }
                    true
                })
                .build(),
        )
        .on_page_load(|webview, payload| {
            // Die Seite lädt bei jeder Navigation neu – Deckkraft erneut setzen.
            if let PageLoadEvent::Finished = payload.event() {
                apply_opacity(webview.app_handle());
            }
        })
        .on_window_event(|window, event| match event {
            // Das Hauptfenster schließt nur ausgeblendet; beendet wird über das Tray-Icon.
            WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                api.prevent_close();
                let _ = window.hide();
            }
            // Ein Widget ist zu (Tray-Haken, X im Bearbeiten-Rahmen): Haken und Merkliste nachführen.
            WindowEvent::Destroyed => {
                if let Some(id) = window.label().strip_prefix("w-") {
                    let app = window.app_handle();
                    set_widget_check(app, id, false);
                    save_open(app, Some(window.label()));
                }
            }
            _ => {}
        })
        .setup(|app| {
            let shortcuts = app.global_shortcut();
            for code in [Code::KeyC, Code::KeyX, Code::KeyE, Code::ArrowUp, Code::ArrowDown] {
                // Belegt schon ein anderes Programm die Kombination, läuft die
                // App trotzdem – nur dieser Hotkey fehlt dann.
                if let Err(error) = shortcuts.register(shortcut(code)) {
                    eprintln!("Hotkey {code:?} nicht verfügbar: {error}");
                }
            }

            // Was beim letzten Mal offen war, geht wieder auf.
            let reopen = load_open(app.handle());

            let widget_items: Vec<(&'static str, CheckMenuItem<Wry>)> = WIDGETS
                .iter()
                .map(|def| {
                    CheckMenuItem::with_id(
                        app,
                        format!("w:{}", def.id),
                        def.title,
                        true,
                        reopen.contains(&def.id),
                        None::<&str>,
                    )
                    .map(|item| (def.id, item))
                })
                .collect::<tauri::Result<_>>()?;
            let widget_refs: Vec<&dyn IsMenuItem<Wry>> =
                widget_items.iter().map(|(_, item)| item as &dyn IsMenuItem<Wry>).collect();
            let widgets_menu = Submenu::with_items(app, "Widgets", true, &widget_refs)?;

            let toggle = MenuItem::with_id(app, "toggle", "Alles ein-/ausblenden (Strg+Alt+C)", true, None::<&str>)?;
            let edit = CheckMenuItem::with_id(app, "edit", "Widgets bearbeiten (Strg+Alt+E)", true, false, None::<&str>)?;
            let click = CheckMenuItem::with_id(app, "click", "Spielmodus: klickdurchlässig (Strg+Alt+X)", true, false, None::<&str>)?;
            let update = MenuItem::with_id(
                app,
                "update",
                format!("Nach Updates suchen (v{})", env!("CARGO_PKG_VERSION")),
                true,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, "quit", "Beenden", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let entries: [&dyn IsMenuItem<Wry>; 7] = [&widgets_menu, &edit, &click, &separator, &toggle, &update, &quit];
            let menu = Menu::with_items(app, &entries)?;

            app.manage(TrayItems { widgets: widget_items, edit, click });

            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("ERLC CAD")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    let id = event.id.as_ref();
                    if let Some(widget) = id.strip_prefix("w:") {
                        if let Some(def) = WIDGETS.iter().find(|def| def.id == widget) {
                            toggle_widget(app, def);
                        }
                        return;
                    }
                    match id {
                        "toggle" => toggle_visible(app),
                        "edit" => toggle_edit(app),
                        "click" => toggle_click_through(app),
                        "update" => {
                            tauri::async_runtime::spawn(check_for_update(app.clone(), true));
                        }
                        "quit" => {
                            app.state::<Overlay>().quitting.store(true, Ordering::Relaxed);
                            app.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_visible(tray.app_handle());
                    }
                })
                .build(app)?;

            for def in WIDGETS.iter().filter(|def| reopen.contains(&def.id)) {
                open_widget(app.handle(), def);
            }

            start_update_checks(app.handle());

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("ERLC CAD Desktop konnte nicht starten");
}
