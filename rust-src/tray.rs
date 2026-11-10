//! Native AppIndicator tray. Libraries are discovered at runtime.
#[cfg(feature = "desktop")]
mod native {
    use gtk::{glib::translate::ToGlibPtr, prelude::*};
    use std::{
        ffi::{CString, c_char, c_void},
        rc::Rc,
    };
    type New = unsafe extern "C" fn(*const c_char, *const c_char, i32) -> *mut c_void;
    type SetMenu = unsafe extern "C" fn(*mut c_void, *mut gtk::ffi::GtkMenu);
    type SetStatus = unsafe extern "C" fn(*mut c_void, i32);
    pub struct Tray {
        indicator: *mut c_void,
        _menu: gtk::Menu,
        open_item: gtk::MenuItem,
        quit_item: gtk::MenuItem,
        status: SetStatus,
        _library: *mut c_void,
    }
    impl Tray {
        pub fn set_labels(&self, open: &str, quit: &str) {
            self.open_item.set_label(open);
            self.quit_item.set_label(quit);
        }
        /// Call on the GTK thread after the desktop event loop has been initialized.
        pub fn new(
            icon: &str,
            open: impl Fn() + 'static,
            quit: impl Fn() + 'static,
        ) -> Option<Self> {
            let bus =
                gtk::gio::bus_get_sync(gtk::gio::BusType::Session, None::<&gtk::gio::Cancellable>)
                    .ok()?;
            let watcher = [
                "org.kde.StatusNotifierWatcher",
                "org.freedesktop.StatusNotifierWatcher",
            ]
            .iter()
            .any(|name| {
                bus.call_sync(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "NameHasOwner",
                    Some(&(*name,).to_variant()),
                    None,
                    gtk::gio::DBusCallFlags::NONE,
                    750,
                    None::<&gtk::gio::Cancellable>,
                )
                .ok()
                .and_then(|v| v.get::<(bool,)>())
                .is_some_and(|v| v.0)
            });
            if !watcher {
                return None;
            }
            unsafe {
                let mut library = std::ptr::null_mut();
                for name in [c"libayatana-appindicator3.so.1", c"libappindicator3.so.1"] {
                    library = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
                    if !library.is_null() {
                        break;
                    }
                }
                if library.is_null() {
                    return None;
                }
                let new_ptr = libc::dlsym(library, c"app_indicator_new".as_ptr());
                let menu_ptr = libc::dlsym(library, c"app_indicator_set_menu".as_ptr());
                let status_ptr = libc::dlsym(library, c"app_indicator_set_status".as_ptr());
                if new_ptr.is_null() || menu_ptr.is_null() || status_ptr.is_null() {
                    libc::dlclose(library);
                    return None;
                }
                let new: New = std::mem::transmute(new_ptr);
                let set_menu: SetMenu = std::mem::transmute(menu_ptr);
                let status: SetStatus = std::mem::transmute(status_ptr);
                let icon = CString::new(icon).ok()?;
                let indicator = new(c"vice".as_ptr(), icon.as_ptr(), 0);
                if indicator.is_null() {
                    libc::dlclose(library);
                    return None;
                }
                let menu = gtk::Menu::new();
                let open_item = gtk::MenuItem::with_label("Open Vice");
                let quit_item = gtk::MenuItem::with_label("Quit Vice");
                let open = Rc::new(open);
                open_item.connect_activate(move |_| open());
                quit_item.connect_activate(move |_| quit());
                menu.append(&open_item);
                menu.append(&gtk::SeparatorMenuItem::new());
                menu.append(&quit_item);
                menu.show_all();
                set_menu(indicator, menu.to_glib_none().0);
                status(indicator, 1);
                Some(Self {
                    indicator,
                    _menu: menu,
                    open_item,
                    quit_item,
                    status,
                    _library: library,
                })
            }
        }
    }
    impl Drop for Tray {
        fn drop(&mut self) {
            unsafe {
                (self.status)(self.indicator, 0);
                gtk::glib::gobject_ffi::g_object_unref(self.indicator.cast()); /* Keep library mapped: GTK may retain registered GTypes. */
            }
        }
    }
}
#[cfg(feature = "desktop")]
pub use native::Tray;
