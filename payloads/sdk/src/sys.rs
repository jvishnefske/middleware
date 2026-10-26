//! The crate's only `unsafe` code: host import declarations, the three safe
//! wrappers around the imports that write into guest memory (`dma_read`,
//! `uart_read`, `i2c_transfer`), the single-threaded `Sync` impl for [`GuestCell`], and the
//! [`payload!`] export macro.
//!
//! Everything here is a thin, logic-free shim. The rest of the crate is
//! `#![deny(unsafe_code)]`; see the crate documentation for the review notes.
//!
//! On wasm32 the functions forward to the `"mw"` imports declared in
//! [`middleware::wasm_abi::IMPORTS`]. On any other target they are inert
//! stubs (`log` ignores its input, `now_ms` and `now_us` return 0, the rest
//! return [`AbiError::Unsupported`](crate::AbiError::Unsupported)) so host
//! unit tests link without a WASM host.
//!
//! [`payload!`]: crate::payload

use core::cell::{RefCell, RefMut};

#[cfg(target_arch = "wasm32")]
mod imports {
    // SAFETY: these declarations match `middleware::wasm_abi::IMPORTS` exactly
    // (all parameters and results `i32`; `abi-check` verifies the built
    // modules against that table). The functions declared `safe` cannot cause
    // undefined behaviour in the guest for any argument values: the host
    // treats pointers as offsets into the exported memory, checks every
    // `(ptr, len)` pair with `wasm_abi::guest_range`, only ever *reads* guest
    // memory during these calls (`spi_transfer` and `uart_write` copy into
    // firmware-owned DMA buffers before returning), and reports problems as
    // negative codes. `dma_read` and `uart_read` *write* `len` bytes at `ptr`,
    // and `i2c_transfer` writes `rx_len` bytes at `rx_ptr`, so they stay
    // `unsafe` and are only called through the wrappers below.
    #[link(wasm_import_module = "mw")]
    unsafe extern "C" {
        pub safe fn log(ptr: i32, len: i32);
        pub safe fn publish(key_ptr: i32, key_len: i32, data_ptr: i32, data_len: i32) -> i32;
        pub safe fn subscribe(key_ptr: i32, key_len: i32) -> i32;
        pub safe fn now_ms() -> i32;
        pub safe fn set_tick(period_ms: i32) -> i32;
        pub safe fn gpio_output(pin: i32) -> i32;
        pub safe fn gpio_input(pin: i32, pull: i32) -> i32;
        pub safe fn gpio_write(pin: i32, level: i32) -> i32;
        pub safe fn gpio_read(pin: i32) -> i32;
        // ABI 2
        pub safe fn now_us() -> i32;
        pub safe fn irq_overflow() -> i32;
        pub safe fn gpio_irq(pin: i32, edge: i32) -> i32;
        pub safe fn timer_start(id: i32, period_us: i32, periodic: i32) -> i32;
        pub safe fn timer_stop(id: i32) -> i32;
        pub safe fn spi_open(
            bus: i32,
            sck: i32,
            mosi: i32,
            miso: i32,
            cs: i32,
            freq_khz: i32,
            mode: i32,
        ) -> i32;
        pub safe fn spi_transfer(bus: i32, tx_ptr: i32, tx_len: i32, rx_len: i32) -> i32;
        pub fn dma_read(channel: i32, ptr: i32, len: i32) -> i32;
        pub safe fn uart_open(port: i32, tx: i32, rx: i32, baud: i32) -> i32;
        pub safe fn uart_write(port: i32, ptr: i32, len: i32) -> i32;
        pub fn uart_read(port: i32, ptr: i32, len: i32) -> i32;
        // ABI 3
        pub safe fn pwm_open(timer: i32, freq_hz: i32) -> i32;
        pub safe fn pwm_set(timer: i32, channel: i32, duty: i32) -> i32;
        pub safe fn pwm_stop(timer: i32) -> i32;
        pub safe fn adc_read(pin: i32) -> i32;
        pub safe fn i2c_open(bus: i32, freq_hz: i32) -> i32;
        pub fn i2c_transfer(
            bus: i32,
            addr: i32,
            tx_ptr: i32,
            tx_len: i32,
            rx_ptr: i32,
            rx_len: i32,
        ) -> i32;
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imports {
    //! Host stand-ins for the imports, used by unit tests. `dma_read`,
    //! `uart_read` and `i2c_transfer` are `unsafe fn` only to match the
    //! wasm32 declarations.
    const UNSUPPORTED: i32 = crate::AbiError::Unsupported as i32;
    pub fn log(_ptr: i32, _len: i32) {}
    pub fn publish(_kp: i32, _kl: i32, _dp: i32, _dl: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn subscribe(_kp: i32, _kl: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn now_ms() -> i32 {
        0
    }
    pub fn set_tick(_p: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn gpio_output(_pin: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn gpio_input(_pin: i32, _pull: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn gpio_write(_pin: i32, _level: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn gpio_read(_pin: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn now_us() -> i32 {
        0
    }
    pub fn irq_overflow() -> i32 {
        UNSUPPORTED
    }
    pub fn gpio_irq(_pin: i32, _edge: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn timer_start(_id: i32, _period_us: i32, _periodic: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn timer_stop(_id: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn spi_open(_b: i32, _s: i32, _o: i32, _i: i32, _c: i32, _f: i32, _m: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn spi_transfer(_b: i32, _p: i32, _tl: i32, _rl: i32) -> i32 {
        UNSUPPORTED
    }
    pub unsafe fn dma_read(_c: i32, _p: i32, _l: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn uart_open(_port: i32, _tx: i32, _rx: i32, _baud: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn uart_write(_port: i32, _p: i32, _l: i32) -> i32 {
        UNSUPPORTED
    }
    pub unsafe fn uart_read(_port: i32, _p: i32, _l: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn pwm_open(_timer: i32, _freq_hz: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn pwm_set(_timer: i32, _channel: i32, _duty: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn pwm_stop(_timer: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn adc_read(_pin: i32) -> i32 {
        UNSUPPORTED
    }
    pub fn i2c_open(_bus: i32, _freq_hz: i32) -> i32 {
        UNSUPPORTED
    }
    pub unsafe fn i2c_transfer(_b: i32, _a: i32, _tp: i32, _tl: i32, _rp: i32, _rl: i32) -> i32 {
        UNSUPPORTED
    }
}

pub(crate) use imports::{
    adc_read, gpio_input, gpio_irq, gpio_output, gpio_read, gpio_write, i2c_open, irq_overflow,
    log, now_ms, now_us, publish, pwm_open, pwm_set, pwm_stop, set_tick, spi_open, spi_transfer,
    subscribe, timer_start, timer_stop, uart_open, uart_write,
};

/// `(ptr, len)` of an exclusively borrowed guest buffer, or `None` if the
/// length does not fit the ABI's `i32` (never for buffers in one 64 KiB page).
///
/// The address is exact on wasm32, where addresses are 32 bits; on the host
/// (tests only) it is truncated, which is harmless because the stubs ignore
/// it.
fn out_buffer(buf: &mut [u8]) -> Option<(i32, i32)> {
    let len = i32::try_from(buf.len()).ok()?;
    let ptr = buf.as_mut_ptr() as usize as u32 as i32;
    Some((ptr, len))
}

/// Copies up to `buf.len()` received SPI bytes of DMA `channel` into `buf`
/// (`dma_read`); returns the import's result.
pub(crate) fn dma_read(channel: i32, buf: &mut [u8]) -> i32 {
    let Some((ptr, len)) = out_buffer(buf) else {
        return crate::AbiError::OutOfBounds.code();
    };
    // SAFETY: `(ptr, len)` describes exactly `buf`, which this function holds
    // as `&mut [u8]` for the whole call, so the host's write of at most `len`
    // bytes at `ptr` lands in memory nothing else references. The host
    // validates the range against linear memory and does not retain the
    // pointer after returning.
    unsafe { imports::dma_read(channel, ptr, len) }
}

/// Copies up to `buf.len()` buffered bytes of UART `port` into `buf`
/// (`uart_read`); returns the import's result.
pub(crate) fn uart_read(port: i32, buf: &mut [u8]) -> i32 {
    let Some((ptr, len)) = out_buffer(buf) else {
        return crate::AbiError::OutOfBounds.code();
    };
    // SAFETY: as for `dma_read`: the host writes at most `len` bytes into
    // `buf`, which is exclusively borrowed for the duration of the call.
    unsafe { imports::uart_read(port, ptr, len) }
}

/// Runs one I2C transfer to `addr` on `bus`: writes `tx`, then reads into
/// `rx` (`i2c_transfer`); returns the import's result.
pub(crate) fn i2c_transfer(bus: i32, addr: i32, tx: &[u8], rx: &mut [u8]) -> i32 {
    let Ok(tx_len) = i32::try_from(tx.len()) else {
        return crate::AbiError::OutOfBounds.code();
    };
    let Some((rx_ptr, rx_len)) = out_buffer(rx) else {
        return crate::AbiError::OutOfBounds.code();
    };
    let tx_ptr = crate::ptr_i32(tx);
    // SAFETY: `(rx_ptr, rx_len)` describes exactly `rx`, held as `&mut [u8]`
    // for the whole call, so the host's write of at most `rx_len` bytes
    // lands in memory nothing else references; `tx` is only read (the host
    // copies it out before the transfer). The host validates both ranges
    // against linear memory and retains neither pointer after returning.
    unsafe { imports::i2c_transfer(bus, addr, tx_ptr, tx_len, rx_ptr, rx_len) }
}

/// A `RefCell` that may be placed in a `static` on single-threaded wasm32.
///
/// [`payload!`](crate::payload) keeps the application state and the receive
/// buffer in `GuestCell`s. Access goes through [`GuestCell::borrow_mut`],
/// which returns `None` instead of panicking if the cell is already borrowed
/// (a re-entrant host call).
pub struct GuestCell<T>(RefCell<T>);

impl<T> GuestCell<T> {
    /// Wraps `value`.
    pub const fn new(value: T) -> Self {
        GuestCell(RefCell::new(value))
    }

    /// Mutably borrows the contents, or `None` if they are already borrowed.
    pub fn borrow_mut(&self) -> Option<RefMut<'_, T>> {
        self.0.try_borrow_mut().ok()
    }

    /// Address of the contents, for handing a buffer to the host.
    pub fn addr(&self) -> usize {
        self.0.as_ptr() as usize
    }
}

// SAFETY: `GuestCell` is only `Sync` on wasm32 without the `atomics` target
// feature. There, a module instance has a single thread of execution and no
// shared memory, so no two threads can ever observe the cell; the `RefCell`
// still enforces the borrow rules dynamically against re-entrant calls.
#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
unsafe impl<T> Sync for GuestCell<T> {}

/// Generates the ABI exports (`init`, `tick`, `mw_alloc`, `on_message`,
/// `on_irq`) for a type implementing [`Payload`](crate::Payload).
///
/// Use it exactly once, at the top level of a `cdylib` crate:
///
/// ```ignore
/// mw_payload::payload!(MyApp);
/// ```
///
/// The expansion holds the state in a `static GuestCell<Option<MyApp>>` and
/// the receive buffer in a `static GuestCell<[u8; MAX_MESSAGE]>`, and forwards
/// each export to the safe functions in [`rt`](crate::rt). The five function
/// exports carry `#[unsafe(no_mangle)]` so the linker emits the fixed ABI
/// names.
#[macro_export]
macro_rules! payload {
    ($ty:ty) => {
        const _: () = {
            static STATE: $crate::GuestCell<::core::option::Option<$ty>> =
                $crate::GuestCell::new(::core::option::Option::None);
            static RX: $crate::GuestCell<[u8; $crate::MAX_MESSAGE]> =
                $crate::GuestCell::new([0; $crate::MAX_MESSAGE]);

            #[allow(unsafe_code)]
            #[unsafe(no_mangle)]
            pub extern "C" fn init() {
                $crate::rt::init::<$ty>(&STATE);
            }

            #[allow(unsafe_code)]
            #[unsafe(no_mangle)]
            pub extern "C" fn tick(now_ms: i32) {
                $crate::rt::tick::<$ty>(&STATE, now_ms);
            }

            #[allow(unsafe_code)]
            #[unsafe(no_mangle)]
            pub extern "C" fn mw_alloc(len: i32) -> i32 {
                $crate::rt::alloc(&RX, len)
            }

            #[allow(unsafe_code)]
            #[unsafe(no_mangle)]
            pub extern "C" fn on_message(slot: i32, ptr: i32, len: i32) {
                $crate::rt::on_message::<$ty>(&STATE, &RX, slot, ptr, len);
            }

            #[allow(unsafe_code)]
            #[unsafe(no_mangle)]
            pub extern "C" fn on_irq(kind: i32, source: i32, value: i32, time_us: i32) {
                $crate::rt::on_irq::<$ty>(&STATE, kind, source, value, time_us);
            }
        };
    };
}
