//! Bajar la prioridad del proceso mientras se construye un índice.

use self::platform::{Original, lower, restore};

/// Reduce la prioridad del proceso de forma temporal. Al caer fuera de ámbito
/// restaura la que había, así que una construcción fallida no deja el shell
/// funcionando a media máquina.
pub struct LowPriority(Option<Original>);

impl LowPriority {
    pub fn set(enabled: bool) -> Self {
        Self(if enabled { Some(lower()) } else { None })
    }
}

impl Drop for LowPriority {
    fn drop(&mut self) {
        if let Some(original) = self.0.take() {
            restore(original);
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetPriorityClass, NORMAL_PRIORITY_CLASS,
        SetPriorityClass,
    };

    pub type Original = u32;

    #[allow(unsafe_code)]
    pub(super) fn lower() -> Original {
        // SAFETY: GetCurrentProcess es un seudohandle válido de por vida y las
        // clases de prioridad son enteros; no se toca memoria ajena.
        unsafe {
            let current = GetPriorityClass(GetCurrentProcess());
            // 0 significa «desconocida»: NORMAL es la única apuesta razonable.
            let original = if current == 0 {
                NORMAL_PRIORITY_CLASS
            } else {
                current
            };
            SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
            original
        }
    }

    #[allow(unsafe_code)]
    pub(super) fn restore(original: Original) {
        // SAFETY: mismo contrato que `lower`.
        unsafe {
            SetPriorityClass(GetCurrentProcess(), original);
        }
    }
}

#[cfg(unix)]
mod platform {
    pub type Original = i32;

    #[allow(unsafe_code)]
    pub(super) fn lower() -> Original {
        // SAFETY: getpid/getpriority/setpriority solo afectan a nuestro
        // proceso; un fallo se ignora y restore() reintenta con el valor guardado.
        unsafe {
            let pid = libc::getpid();
            let original = libc::getpriority(libc::PRIO_PROCESS, pid);
            libc::setpriority(libc::PRIO_PROCESS, pid, 10);
            original
        }
    }

    #[allow(unsafe_code)]
    pub(super) fn restore(original: Original) {
        // SAFETY: idem.
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, libc::getpid(), original);
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    pub type Original = ();

    pub(super) fn lower() -> Original {}

    pub(super) fn restore(_original: Original) {}
}
