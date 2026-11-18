//! Trae al frente la ventana que ya está abierta.
//!
//! Raven no tiene una función para desminimizar y activar una ventana, así que
//! en X11 se lo pedimos al gestor de ventanas con el mensaje EWMH
//! `_NET_ACTIVE_WINDOW`: restaura, sube y enfoca sin desmaximizar. En Wayland
//! el protocolo no permite que un cliente haga eso: se intenta lo que ofrece
//! Raven y nada más.
//!
//! libX11 se carga en tiempo de ejecución para no añadirla como dependencia
//! del binario: si no está (por ejemplo, un Raven solo de Wayland), se usa la
//! alternativa de Raven.

use raven::*;
use std::ffi::c_char;
use std::os::raw::{c_int, c_long, c_ulong, c_void};
use std::ptr;
use std::sync::OnceLock;

const CLIENT_MESSAGE: c_int = 33;
/// `SubstructureNotifyMask | SubstructureRedirectMask`: la máscara con la que
/// se envían los mensajes EWMH a la ventana raíz.
const ROOT_EVENT_MASK: c_long = (1 << 19) | (1 << 20);
const CURRENT_TIME: c_ulong = 0;
/// `source indication` 1: la petición la hace una aplicación.
const SOURCE_APPLICATION: c_long = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct ClientMessageData {
    l: [c_long; 5],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ClientMessageEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut c_void,
    window: c_ulong,
    message_type: c_ulong,
    format: c_int,
    data: ClientMessageData,
}

/// Mismo tamaño que un `XEvent` de Xlib (24 `long` en 64 bits); XSendEvent
/// recibe un puntero a la unión completa.
#[repr(C)]
union XEvent {
    client: ClientMessageEvent,
    pad: [c_long; 24],
}

type XInternAtom = unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_ulong;
type XSendEvent =
    unsafe extern "C" fn(*mut c_void, c_ulong, c_int, c_long, *mut XEvent) -> c_int;
type XDefaultRootWindow = unsafe extern "C" fn(*mut c_void) -> c_ulong;
type XFlush = unsafe extern "C" fn(*mut c_void) -> c_int;

struct Xlib {
    intern_atom: XInternAtom,
    send_event: XSendEvent,
    default_root_window: XDefaultRootWindow,
    flush: XFlush,
}

unsafe fn symbol<T: Copy>(library: *mut c_void, name: &std::ffi::CStr) -> Option<T> {
    let pointer = libc::dlsym(library, name.as_ptr());
    if pointer.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&pointer))
    }
}

fn load_xlib() -> Option<Xlib> {
    unsafe {
        let library = libc::dlopen(
            c"libX11.so.6".as_ptr(),
            libc::RTLD_LAZY | libc::RTLD_LOCAL,
        );
        if library.is_null() {
            return None;
        }
        Some(Xlib {
            intern_atom: symbol(library, c"XInternAtom")?,
            send_event: symbol(library, c"XSendEvent")?,
            default_root_window: symbol(library, c"XDefaultRootWindow")?,
            flush: symbol(library, c"XFlush")?,
        })
    }
}

fn xlib() -> Option<&'static Xlib> {
    static XLIB: OnceLock<Option<Xlib>> = OnceLock::new();
    XLIB.get_or_init(load_xlib).as_ref()
}

/// Deja la ventana delante del usuario.
///
/// En X11 pide la activación al gestor de ventanas; si no se puede, al menos
/// la muestra y la sube. En Wayland no hay equivalente público.
pub fn bring_to_front(window: Rid) {
    unsafe {
        let mut native = RNativeWindow {
            system: R_WINDOW_SYSTEM_NONE,
            display: ptr::null_mut(),
            window: 0,
            visual: 0,
            transparent: R_FALSE,
        };
        let is_x11 = Rwindow_get_native_handle(window, &mut native) != R_FALSE
            && native.system == R_WINDOW_SYSTEM_X11
            && !native.display.is_null()
            && native.window != 0;

        if !is_x11 {
            Rwindow_restore(window);
            Rwindow_show(window);
            return;
        }

        if let Some(xlib) = xlib() {
            let display = native.display;
            let active_window =
                (xlib.intern_atom)(display, c"_NET_ACTIVE_WINDOW".as_ptr(), R_FALSE);
            if active_window != 0 {
                let mut event = XEvent { pad: [0; 24] };
                event.client = ClientMessageEvent {
                    type_: CLIENT_MESSAGE,
                    serial: 0,
                    send_event: R_TRUE,
                    display,
                    window: native.window as c_ulong,
                    message_type: active_window,
                    format: 32,
                    data: ClientMessageData {
                        l: [SOURCE_APPLICATION, CURRENT_TIME as c_long, 0, 0, 0],
                    },
                };
                let root = (xlib.default_root_window)(display);
                (xlib.send_event)(display, root, R_FALSE, ROOT_EVENT_MASK, &mut event);
                (xlib.flush)(display);
            }
        }

        // Aunque el gestor de ventanas no atienda EWMH, al menos se sube.
        Rwindow_show(window);
    }
}
