//! macOS integration: opening photos from Finder ("Open With", or dropping them on the Dock
//! icon), and the "Presets" menu in the menu bar.
//!
//! macOS delivers those as an "open documents" Apple Event, which winit doesn't handle. We
//! install our own handler as the app finishes launching (Apple's recommended moment, so it's
//! in place before the first event arrives when the app is launched *by* the open) and hand the
//! paths to the UI through a queue.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use eframe::egui;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSNotificationCenter, NSString};

use crate::palette::Command;

/// Four-character Apple Event codes: 'aevt', 'odoc', '----'.
const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const OPEN_DOCUMENTS: u32 = u32::from_be_bytes(*b"odoc");
const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

static OPENED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static MENU_CLICKS: Mutex<Vec<Command>> = Mutex::new(Vec::new());
static MENU_INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CONTEXT: OnceLock<egui::Context> = OnceLock::new();

/// The "Presets" menu: each item runs the palette command of the same name. `None` = separator.
const PRESETS_MENU: [Option<(Command, &str)>; 5] = [
    Some((Command::ImportPresetFiles, "Import Presets…")),
    Some((Command::ImportPresetFolder, "Import Preset Folder…")),
    Some((Command::ShowPresetsFolder, "Show Presets Folder")),
    None,
    Some((Command::GetFreePresets, "Get Free Presets…")),
];

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "BPPhotosOpenHandler"]
    struct OpenHandler;

    impl OpenHandler {
        #[unsafe(method(willFinishLaunching:))]
        fn will_finish_launching(&self, _note: &NSNotification) {
            let manager = NSAppleEventManager::sharedAppleEventManager();
            let handler: &AnyObject = self.as_ref();
            // Replaces AppKit's default handler, which would ask winit's delegate (and fail).
            let _: () = unsafe {
                msg_send![&manager, setEventHandler: handler, andSelector: sel!(handleOpen:withReply:),
                    forEventClass: CORE_EVENT_CLASS, andEventID: OPEN_DOCUMENTS]
            };
        }

        #[unsafe(method(menuAction:))]
        fn menu_action(&self, item: &NSMenuItem) {
            if let Some(Some((command, _))) = PRESETS_MENU.get(item.tag() as usize) {
                MENU_CLICKS.lock().unwrap().push(*command);
                if let Some(ctx) = CONTEXT.get() {
                    ctx.request_repaint();
                }
            }
        }

        #[unsafe(method(handleOpen:withReply:))]
        fn handle_open(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            let files: Option<Retained<NSAppleEventDescriptor>> = unsafe { msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT] };
            let Some(files) = files else { return };
            // A list of file URLs (1-based), or a single one.
            let count = files.numberOfItems();
            let items: Vec<Retained<NSAppleEventDescriptor>> = if count > 0 {
                (1..=count).filter_map(|i| files.descriptorAtIndex(i)).collect()
            } else {
                vec![files]
            };
            let paths: Vec<PathBuf> = items
                .iter()
                .filter_map(|d| d.fileURLValue())
                .filter_map(|url| url.path())
                .map(|p| PathBuf::from(p.to_string()))
                .collect();
            if paths.is_empty() {
                return;
            }
            OPENED.lock().unwrap().extend(paths);
            if let Some(ctx) = CONTEXT.get() {
                ctx.request_repaint();
            }
        }
    }
);

/// Call before starting the event loop.
pub fn install() {
    let handler: Retained<OpenHandler> = unsafe { msg_send![OpenHandler::alloc(), init] };
    let name = NSString::from_str("NSApplicationWillFinishLaunchingNotification");
    unsafe {
        NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
            handler.as_ref(),
            sel!(willFinishLaunching:),
            Some(&name),
            None,
        );
    }
    // Needed for the life of the app (the notification centre doesn't keep it alive).
    std::mem::forget(handler);
}

/// Lets the handler wake the UI when files arrive.
pub fn set_context(ctx: &egui::Context) {
    _ = CONTEXT.set(ctx.clone());
}

/// Files macOS asked us to open since the last call.
pub fn take_opened() -> Vec<PathBuf> {
    std::mem::take(&mut *OPENED.lock().unwrap())
}

/// Adds the "Presets" menu to the menu bar (once; call from the UI thread after launch).
pub fn install_menu() {
    use std::sync::atomic::Ordering;
    let Some(mtm) = MainThreadMarker::new() else { return };
    if MENU_INSTALLED.load(Ordering::Relaxed) {
        return;
    }
    let Some(main_menu) = NSApplication::sharedApplication(mtm).mainMenu() else { return };
    MENU_INSTALLED.store(true, Ordering::Relaxed);

    let target: Retained<OpenHandler> = unsafe { msg_send![OpenHandler::alloc(), init] };
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Presets"));
    for (tag, entry) in PRESETS_MENU.iter().enumerate() {
        let Some((_, title)) = entry else {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            continue;
        };
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), Some(sel!(menuAction:)), &NSString::from_str(""))
        };
        unsafe { item.setTarget(Some(target.as_ref())) };
        item.setTag(tag as isize);
        menu.addItem(&item);
    }
    let top = NSMenuItem::new(mtm);
    top.setSubmenu(Some(&menu));
    main_menu.addItem(&top);
    // The standard app menu is titled after the executable ("About bp_photos"…): use the app name.
    if let Some(app_menu) = main_menu.itemAtIndex(0).and_then(|it| it.submenu()) {
        for item in (0..app_menu.numberOfItems()).filter_map(|i| app_menu.itemAtIndex(i)) {
            let title = item.title().to_string();
            if title.contains("bp_photos") {
                item.setTitle(&NSString::from_str(&title.replace("bp_photos", "BP Photos")));
            }
        }
    }
    // Menu items don't keep their target alive.
    std::mem::forget(target);
}

/// Menu items chosen since the last call.
pub fn take_menu_commands() -> Vec<Command> {
    std::mem::take(&mut *MENU_CLICKS.lock().unwrap())
}
