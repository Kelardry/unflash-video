use std::ffi::c_int;
use std::ffi::c_uint;
use strum::FromRepr;

#[derive(Clone, Copy, PartialEq, Eq, FromRepr, Debug)]
#[repr(u8)]
#[non_exhaustive]
pub enum Rav1dError {
    /// This represents a generic `rav1d` error.
    /// It has nothing to do with the other `errno`-based ones
    /// (and that's why it's not all caps like the other ones).
    ///
    /// Normally `EPERM = 1`, but `dav1d` never uses `EPERM`,
    /// but does use `-1`, as opposed to the normal `DAV1D_ERR(E*)`.
    ///
    /// Also Note that this forces `0` to be the niche,
    /// which is more optimal since `0` is no error for [`Dav1dResult`].
    EGeneric = 1,

    #[cfg(not(target_family = "wasm"))]
    ENOENT = libc::ENOENT as u8,
    #[cfg(target_family = "wasm")]
    ENOENT = 2,
    #[cfg(not(target_family = "wasm"))]
    EIO = libc::EIO as u8,
    #[cfg(target_family = "wasm")]
    EIO = 5,
    #[cfg(not(target_family = "wasm"))]
    EAGAIN = libc::EAGAIN as u8,
    #[cfg(target_family = "wasm")]
    EAGAIN = 11,
    #[cfg(not(target_family = "wasm"))]
    ENOMEM = libc::ENOMEM as u8,
    #[cfg(target_family = "wasm")]
    ENOMEM = 12,
    #[cfg(not(target_family = "wasm"))]
    EINVAL = libc::EINVAL as u8,
    #[cfg(target_family = "wasm")]
    EINVAL = 22,
    #[cfg(not(target_family = "wasm"))]
    ERANGE = libc::ERANGE as u8,
    #[cfg(target_family = "wasm")]
    ERANGE = 34,
    #[cfg(not(target_family = "wasm"))]
    ENOPROTOOPT = libc::ENOPROTOOPT as u8,
    #[cfg(target_family = "wasm")]
    ENOPROTOOPT = 92,
}

pub type Rav1dResult<T = ()> = Result<T, Rav1dError>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct Dav1dResult(pub c_int);

impl From<Rav1dResult> for Dav1dResult {
    #[inline]
    fn from(value: Rav1dResult) -> Self {
        // Doing the `-` negation on both branches
        // makes the code short and branchless.
        Dav1dResult(
            -(match value {
                Ok(()) => 0,
                Err(e) => e as c_int,
            }),
        )
    }
}

impl From<Rav1dResult<c_uint>> for Dav1dResult {
    #[inline]
    fn from(value: Rav1dResult<c_uint>) -> Self {
        Dav1dResult(match value {
            Ok(value) => value as c_int,
            Err(e) => e as c_int,
        })
    }
}

impl TryFrom<Dav1dResult> for Rav1dResult {
    type Error = Dav1dResult;

    #[inline]
    fn try_from(value: Dav1dResult) -> Result<Self, Self::Error> {
        match value.0 {
            0 => Ok(Ok(())),
            e => {
                let e = (-e).try_into().map_err(|_| value)?;
                let e = Rav1dError::from_repr(e).ok_or(value)?;
                Ok(Err(e))
            }
        }
    }
}
