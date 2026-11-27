//! Owning ALSA control handle and confined mixer ABI.
use libloading::Library;
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_void};
const IFACE_MIXER: c_int = 2;
const TYPE_BOOLEAN: c_int = 1;
const TYPE_INTEGER: c_int = 2;
const TYPE_ENUMERATED: c_int = 3;
const UNITY: c_long = 255;

struct Api {
    _library: Library,
    open: unsafe extern "C" fn(*mut *mut c_void, *const c_char, c_int) -> c_int,
    close: unsafe extern "C" fn(*mut c_void) -> c_int,
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
    info_malloc: unsafe extern "C" fn(*mut *mut c_void) -> c_int,
    info_free: unsafe extern "C" fn(*mut c_void),
    info_clear: unsafe extern "C" fn(*mut c_void),
    info_set_interface: unsafe extern "C" fn(*mut c_void, c_int),
    info_set_name: unsafe extern "C" fn(*mut c_void, *const c_char),
    info: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
    info_get_type: unsafe extern "C" fn(*const c_void) -> c_int,
    info_get_count: unsafe extern "C" fn(*const c_void) -> c_uint,
    info_get_min: unsafe extern "C" fn(*const c_void) -> c_long,
    info_get_max: unsafe extern "C" fn(*const c_void) -> c_long,
    value_malloc: unsafe extern "C" fn(*mut *mut c_void) -> c_int,
    value_free: unsafe extern "C" fn(*mut c_void),
    value_clear: unsafe extern "C" fn(*mut c_void),
    value_set_interface: unsafe extern "C" fn(*mut c_void, c_int),
    value_set_name: unsafe extern "C" fn(*mut c_void, *const c_char),
    value_set_integer: unsafe extern "C" fn(*mut c_void, c_uint, c_long),
    value_get_integer: unsafe extern "C" fn(*const c_void, c_uint) -> c_long,
    value_set_enumerated: unsafe extern "C" fn(*mut c_void, c_uint, c_uint),
    value_get_enumerated: unsafe extern "C" fn(*const c_void, c_uint) -> c_uint,
    value_set_boolean: unsafe extern "C" fn(*mut c_void, c_uint, c_long),
    value_get_boolean: unsafe extern "C" fn(*const c_void, c_uint) -> c_int,
    write: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
    read: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
}

impl Api {
    fn load(_blocking: &crate::rt::Blocking) -> Result<Self, String> {
        let library = unsafe { Library::new("libasound.so.2") }.map_err(|e| format!("libasound mixer: {e}"))?;
        macro_rules! sym {
            ($name:literal) => {
                *unsafe { library.get(concat!($name, "\0").as_bytes()) }.map_err(|e| format!("{}: {e}", $name))?
            };
        }
        Ok(Self {
            open: sym!("snd_ctl_open"), close: sym!("snd_ctl_close"), strerror: sym!("snd_strerror"),
            info_malloc: sym!("snd_ctl_elem_info_malloc"), info_free: sym!("snd_ctl_elem_info_free"),
            info_clear: sym!("snd_ctl_elem_info_clear"), info_set_interface: sym!("snd_ctl_elem_info_set_interface"),
            info_set_name: sym!("snd_ctl_elem_info_set_name"), info: sym!("snd_ctl_elem_info"),
            info_get_type: sym!("snd_ctl_elem_info_get_type"), info_get_count: sym!("snd_ctl_elem_info_get_count"),
            info_get_min: sym!("snd_ctl_elem_info_get_min"), info_get_max: sym!("snd_ctl_elem_info_get_max"),
            value_malloc: sym!("snd_ctl_elem_value_malloc"), value_free: sym!("snd_ctl_elem_value_free"),
            value_clear: sym!("snd_ctl_elem_value_clear"), value_set_interface: sym!("snd_ctl_elem_value_set_interface"),
            value_set_name: sym!("snd_ctl_elem_value_set_name"), value_set_integer: sym!("snd_ctl_elem_value_set_integer"),
            value_get_integer: sym!("snd_ctl_elem_value_get_integer"),
            value_set_enumerated: sym!("snd_ctl_elem_value_set_enumerated"),
            value_get_enumerated: sym!("snd_ctl_elem_value_get_enumerated"),
            value_set_boolean: sym!("snd_ctl_elem_value_set_boolean"),
            value_get_boolean: sym!("snd_ctl_elem_value_get_boolean"),
            write: sym!("snd_ctl_elem_write"), read: sym!("snd_ctl_elem_read"),
            _library: library,
        })
    }

    fn check(&self, operation: &str, rc: c_int) -> Result<(), String> {
        if rc >= 0 { return Ok(()); }
        let reason = unsafe { CStr::from_ptr((self.strerror)(rc)) }.to_string_lossy();
        Err(format!("{operation}: {reason}"))
    }
}

pub(super) struct AlsaControls<'a> {
    blocking: &'a crate::rt::Blocking,
    api: Api,
    ctl: *mut c_void,
    info: *mut c_void,
    value: *mut c_void,
}

impl<'a> AlsaControls<'a> {
    pub(super) fn open(blocking: &'a crate::rt::Blocking) -> Result<Self, String> {
        let mut this = Self { blocking, api: Api::load(blocking)?, ctl: std::ptr::null_mut(), info: std::ptr::null_mut(), value: std::ptr::null_mut() };
        this.api.check("open hw:Q2 mixer", unsafe { (this.api.open)(&mut this.ctl, c"hw:Q2".as_ptr(), 0) })?;
        this.api.check("allocate mixer info", unsafe { (this.api.info_malloc)(&mut this.info) })?;
        this.api.check("allocate mixer value", unsafe { (this.api.value_malloc)(&mut this.value) })?;
        Ok(this)
    }

    fn select_value(&mut self, _blocking: &crate::rt::Blocking, dac: usize) {
        let name = CString::new(format!("DAC{dac} Master Playback Volume")).unwrap();
        unsafe {
            (self.api.value_clear)(self.value);
            (self.api.value_set_interface)(self.value, IFACE_MIXER);
            (self.api.value_set_name)(self.value, name.as_ptr());
        }
    }
}

impl<'a> AlsaControls<'a> {
    pub(super) fn validate(&mut self, _blocking: &crate::rt::Blocking, dac: usize) -> Result<(), String> {
        let name = CString::new(format!("DAC{dac} Master Playback Volume")).unwrap();
        unsafe {
            (self.api.info_clear)(self.info);
            (self.api.info_set_interface)(self.info, IFACE_MIXER);
            (self.api.info_set_name)(self.info, name.as_ptr());
            self.api.check(&format!("DAC{dac} mixer info"), (self.api.info)(self.ctl, self.info))?;
            if (self.api.info_get_type)(self.info) != TYPE_INTEGER ||
                (self.api.info_get_count)(self.info) != 2 ||
                (self.api.info_get_min)(self.info) != 0 ||
                (self.api.info_get_max)(self.info) != UNITY {
                return Err(format!("unexpected DAC{dac} playback volume control"));
            }
        }
        Ok(())
    }

    pub(super) fn write(&mut self, blocking: &crate::rt::Blocking, dac: usize, value: c_long) -> Result<(), String> {
        self.select_value(blocking, dac);
        unsafe {
            (self.api.value_set_integer)(self.value, 0, value);
            (self.api.value_set_integer)(self.value, 1, value);
            self.api.check(&format!("DAC{dac} mixer write"), (self.api.write)(self.ctl, self.value))
        }
    }

    pub(super) fn read(&mut self, blocking: &crate::rt::Blocking, dac: usize) -> Result<[c_long; 2], String> {
        self.select_value(blocking, dac);
        unsafe {
            self.api.check(&format!("DAC{dac} mixer read"), (self.api.read)(self.ctl, self.value))?;
            Ok([(self.api.value_get_integer)(self.value, 0), (self.api.value_get_integer)(self.value, 1)])
        }
    }
}

impl<'a> AlsaControls<'a> {
    /// Whether the card has a single-value enumerated control named `name`.
    pub(super) fn has(&mut self, blocking: &crate::rt::Blocking, name: &str) -> bool {
        self.select_named(blocking, name, TYPE_ENUMERATED).is_ok()
    }

    /// Checks that `name` is a single-value control of `kind`, then selects it.
    fn select_named(&mut self, _blocking: &crate::rt::Blocking, name: &str, kind: c_int) -> Result<(), String> {
        let c = CString::new(name).map_err(|_| "control name".to_string())?;
        unsafe {
            (self.api.info_clear)(self.info);
            (self.api.info_set_interface)(self.info, IFACE_MIXER);
            (self.api.info_set_name)(self.info, c.as_ptr());
            self.api.check(&format!("{name} info"), (self.api.info)(self.ctl, self.info))?;
            if (self.api.info_get_type)(self.info) != kind || (self.api.info_get_count)(self.info) != 1 {
                return Err(format!("unexpected {name} control"));
            }
            (self.api.value_clear)(self.value);
            (self.api.value_set_interface)(self.value, IFACE_MIXER);
            (self.api.value_set_name)(self.value, c.as_ptr());
        }
        Ok(())
    }
}

impl<'a> AlsaControls<'a> {
    pub(super) fn write_enum(&mut self, blocking: &crate::rt::Blocking, name: &str, item: c_uint) -> Result<(), String> {
        self.select_named(blocking, name, TYPE_ENUMERATED)?;
        unsafe {
            (self.api.value_set_enumerated)(self.value, 0, item);
            self.api.check(&format!("{name} write"), (self.api.write)(self.ctl, self.value))?;
            self.api.check(&format!("{name} read"), (self.api.read)(self.ctl, self.value))?;
            if (self.api.value_get_enumerated)(self.value, 0) != item { return Err(format!("{name} readback mismatch")); }
        }
        Ok(())
    }
    pub(super) fn write_bool(&mut self, blocking: &crate::rt::Blocking, name: &str, on: bool) -> Result<(), String> {
        self.select_named(blocking, name, TYPE_BOOLEAN)?;
        unsafe {
            (self.api.value_set_boolean)(self.value, 0, c_long::from(on));
            self.api.check(&format!("{name} write"), (self.api.write)(self.ctl, self.value))?;
            self.api.check(&format!("{name} read"), (self.api.read)(self.ctl, self.value))?;
            if ((self.api.value_get_boolean)(self.value, 0) != 0) != on { return Err(format!("{name} readback mismatch")); }
        }
        Ok(())
    }
}

impl AlsaControls<'_> {
    fn close(&mut self, _blocking: &crate::rt::Blocking) {
        unsafe {
            if !self.value.is_null() { (self.api.value_free)(self.value); }
            if !self.info.is_null() { (self.api.info_free)(self.info); }
            if !self.ctl.is_null() { (self.api.close)(self.ctl); }
        }
    }
}


impl Drop for AlsaControls<'_> {
    fn drop(&mut self) {
        let blocking = self.blocking;
        self.close(blocking);
    }
}
