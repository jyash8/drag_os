#![no_std]
#![allow(unused)]
// Names mirror Linux's drm headers (e.g. DRM_CONNECTOR_TYPE_eDP) so values
// stay eyeball-compatible with upstream; hence non-upper-case globals.
#![allow(non_upper_case_globals)]

// ── ioctl numbers ────────────────────────────────────────────────────────────
//
// Every number below is the number *real userspace sends*. libdrm and Mesa
// expand <drm/*.h> macros and pass the result straight to ioctl(2); there is no
// negotiation. A mismatch is therefore not a cosmetic difference:
//
//   * if nothing else claims the number, the call lands in the dispatcher's
//     catch-all and the client sees an error it cannot explain;
//   * if two unrelated ioctls share a number, the call is silently serviced by
//     the *wrong handler* and returns success while doing something else. That
//     is how MODE_GETGAMMA came to alias MODE_CURSOR: a client cursor update
//     returned 0 having done nothing at all.
//
// So no number here is a hand-written literal. Each is computed the way
// <asm-generic/ioctl.h> computes it, from the direction, the request number,
// and `size_of` of the very struct this kernel reads the payload from — then
// pinned to the value upstream Linux defines by the `abi_pin!` block at the
// bottom of this section. A struct whose layout drifts from Linux's therefore
// changes the encoded size *and* fails the build, instead of silently
// misparsing every submit.
//
// Direction bits are transcribed from <drm/drm.h>; the pinned values come from
// the same headers a userspace build would use.

const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + 8;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + 8;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + 14;
const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
/// and the client fills them with a pointer and 0x10, so they are certainly not The kernel's dispatcher matches the full 32-bit encoding, so a direction
/// and the client fills them with a pointer and 0x10, so they are certainly not bit that disagrees with userspace is just as fatal as a wrong `nr`.
const IOC_RW: u32 = IOC_READ | IOC_WRITE;
const DRM_TYPE: u32 = b'd' as u32;

const fn ioc(dir: u32, typ: u32, nr: u32, size: u32) -> u64 {
    ((dir << IOC_DIRSHIFT) | (typ << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT))
        as u64
}

/// and the client fills them with a pointer and 0x10, so they are certainly not Encode a DRM ioctl for a payload of `size` bytes.
const fn drm_ioctl(dir: u32, nr: u32, size: u32) -> u64 {
    ioc(dir, DRM_TYPE, nr, size)
}

/// and the client fills them with a pointer and 0x10, so they are certainly not Encode a DRM ioctl whose payload is exactly `T`, so the encoded size can
/// and the client fills them with a pointer and 0x10, so they are certainly not never disagree with the struct the handler dereferences.
const fn drm_ioctl_t<T>(dir: u32, nr: u32) -> u64 {
    drm_ioctl(dir, nr, core::mem::size_of::<T>() as u32)
}

pub const DRM_IOCTL_VERSION: u64 = drm_ioctl_t::<DrmVersion>(IOC_RW, 0x00);
pub const DRM_IOCTL_GET_CAP: u64 = drm_ioctl_t::<DrmGetCap>(IOC_RW, 0x0C);
pub const DRM_IOCTL_SET_CLIENT_CAP: u64 = drm_ioctl_t::<DrmSetClientCap>(IOC_WRITE, 0x0D);
pub const DRM_IOCTL_GEM_CLOSE: u64 = drm_ioctl_t::<DrmGemClose>(IOC_WRITE, 0x09);
pub const DRM_IOCTL_AUTH_MAGIC: u64 = drm_ioctl(IOC_WRITE, 0x11, core::mem::size_of::<u32>() as u32);
/// and the client fills them with a pointer and 0x10, so they are certainly not Takes no argument upstream, so the encoded size is zero. libdrm passes a
/// and the client fills them with a pointer and 0x10, so they are certainly not pointer regardless; we simply do not read it.
pub const DRM_IOCTL_SET_MASTER: u64 = drm_ioctl(IOC_NONE, 0x1E, 0);
pub const DRM_IOCTL_DROP_MASTER: u64 = drm_ioctl(IOC_NONE, 0x1F, 0);

pub const DRM_IOCTL_MODE_GETRESOURCES: u64 = drm_ioctl_t::<DrmModeCardRes>(IOC_RW, 0xA0);
pub const DRM_IOCTL_MODE_GETCRTC: u64 = drm_ioctl_t::<DrmModeCrtc>(IOC_RW, 0xA1);
pub const DRM_IOCTL_MODE_SETCRTC: u64 = drm_ioctl_t::<DrmModeSetCrtc>(IOC_RW, 0xA2);
pub const DRM_IOCTL_MODE_CURSOR: u64 = drm_ioctl_t::<DrmModeCursor>(IOC_RW, 0xA3);
pub const DRM_IOCTL_MODE_GETGAMMA: u64 = drm_ioctl(IOC_RW, 0xA4, 0x20);
pub const DRM_IOCTL_MODE_SETGAMMA: u64 = drm_ioctl(IOC_RW, 0xA5, 0x20);
pub const DRM_IOCTL_MODE_GETENCODER: u64 = drm_ioctl_t::<DrmModeEncoder>(IOC_RW, 0xA6);
pub const DRM_IOCTL_MODE_GETCONNECTOR: u64 = drm_ioctl_t::<DrmModeGetConnector>(IOC_RW, 0xA7);
pub const DRM_IOCTL_MODE_GETPROPERTY: u64 = drm_ioctl_t::<DrmModeGetProperty>(IOC_RW, 0xAA);
pub const DRM_IOCTL_MODE_GETPROPBLOB: u64 = drm_ioctl_t::<DrmModeGetPropBlob>(IOC_RW, 0xAC);
pub const DRM_IOCTL_MODE_ADDFB: u64 = drm_ioctl_t::<DrmModeFbCmd>(IOC_RW, 0xAE);
pub const DRM_IOCTL_MODE_RMFB: u64 = drm_ioctl_t::<DrmModeDestroyDumb>(IOC_RW, 0xAF);
pub const DRM_IOCTL_MODE_PAGE_FLIP: u64 = drm_ioctl_t::<DrmModePageFlip>(IOC_RW, 0xB0);
pub const DRM_IOCTL_MODE_DIRTYFB: u64 = drm_ioctl_t::<DrmModeFbDirtyCmd>(IOC_RW, 0xB1);
pub const DRM_IOCTL_MODE_CREATE_DUMB: u64 = drm_ioctl_t::<DrmModeCreateDumb>(IOC_RW, 0xB2);
pub const DRM_IOCTL_MODE_MAP_DUMB: u64 = drm_ioctl_t::<DrmModeMapDumb>(IOC_RW, 0xB3);
pub const DRM_IOCTL_MODE_DESTROY_DUMB: u64 = drm_ioctl_t::<DrmModeDestroyDumb>(IOC_RW, 0xB4);
pub const DRM_IOCTL_MODE_GETPLANERESOURCES: u64 = drm_ioctl_t::<DrmModeGetPlaneRes>(IOC_RW, 0xB5);
pub const DRM_IOCTL_MODE_GETPLANE: u64 = drm_ioctl_t::<DrmModeGetPlane>(IOC_RW, 0xB6);
pub const DRM_IOCTL_MODE_SETPLANE: u64 = drm_ioctl_t::<DrmModeSetPlane>(IOC_RW, 0xB7);
pub const DRM_IOCTL_MODE_ADDFB2: u64 = drm_ioctl_t::<DrmModeFbCmd2>(IOC_RW, 0xB8);
pub const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: u64 = drm_ioctl_t::<DrmModeObjGetProperties>(IOC_RW, 0xB9);
pub const DRM_IOCTL_MODE_ATOMIC: u64 = drm_ioctl_t::<DrmModeAtomic>(IOC_RW, 0xBC);

pub const DRM_IOCTL_PRIME_HANDLE_TO_FD: u64 = drm_ioctl_t::<DrmPrimeHandle>(IOC_RW, 0x2D);
pub const DRM_IOCTL_PRIME_FD_TO_HANDLE: u64 = drm_ioctl_t::<DrmPrimeHandle>(IOC_RW, 0x2E);

pub const DRM_IOCTL_SYNCOBJ_CREATE: u64 = drm_ioctl_t::<DrmSyncobjCreate>(IOC_RW, 0xBF);
pub const DRM_IOCTL_SYNCOBJ_DESTROY: u64 = drm_ioctl_t::<DrmSyncobjDestroy>(IOC_RW, 0xC0);
pub const DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD: u64 = drm_ioctl_t::<DrmSyncobjHandle>(IOC_RW, 0xC1);
pub const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: u64 = drm_ioctl_t::<DrmSyncobjHandle>(IOC_RW, 0xC2);
pub const DRM_IOCTL_SYNCOBJ_WAIT: u64 = drm_ioctl_t::<DrmSyncobjWait>(IOC_RW, 0xC3);
pub const DRM_IOCTL_MODE_CREATEPROPBLOB: u64 =
    drm_ioctl_t::<DrmModeCreateBlob>(IOC_RW, 0xBD);
pub const DRM_IOCTL_MODE_DESTROYPROPBLOB: u64 =
    drm_ioctl_t::<DrmModeDestroyBlob>(IOC_RW, 0xBE);
// ponytail: there is deliberately NO DRM_IOCTL_SYNCOBJ_WAIT_BINARY.
//
// It used to be here, aliased to DRM_IOCTL_MODE_CREATEPROPBLOB, which was wrong
// twice over. There is no such ioctl in the userspace ABI: `<drm/drm.h>` defines
// SYNCOBJ_CREATE(0xBF) .. SYNCOBJ_SIGNAL(0xC5) and nothing more, so libdrm can
// never emit WAIT_BINARY. And 0xC6 is already MODE_CREATE_LEASE, not a syncobj
// nr. So the constant named a real ioctl while carrying an unrelated one's
// number, and abi_pin! "verified" it against that same wrong value -- a green
// check that could never fail.
//
// It also had no consumer: no dispatcher arm and no other reference. It was a
// trap for whoever later tried to implement it, who would have found 0xBD taken.
// Removed rather than redefined.
pub const DRM_IOCTL_SYNCOBJ_RESET: u64 = drm_ioctl_t::<DrmSyncobjArray>(IOC_RW, 0xC4);
pub const DRM_IOCTL_SYNCOBJ_SIGNAL: u64 = drm_ioctl_t::<DrmSyncobjArray>(IOC_RW, 0xC5);

// virtio-gpu. Note there is no SETPARAM and no CONTEXT_CREATE/DESTROY in the
// modern numbering: nr 0x45/0x46 are RESOURCE_INFO and TRANSFER_FROM_HOST, and
// nr 0x47 is TRANSFER_TO_HOST. The old kernel here had CONTEXT_CREATE=0x45,
// CONTEXT_DESTROY=0x46 and WAIT=0x47, so a client's RESOURCE_INFO query
// destroyed a context and a TRANSFER_TO_HOST freed the wait struct instead of
// waiting on it. Numbers are as <drm/virtgpu_drm.h> defines them.
pub const DRM_IOCTL_VIRTGPU_MAP: u64 = drm_ioctl_t::<DrmVirtgpuMap>(IOC_RW, 0x41);
pub const DRM_IOCTL_VIRTGPU_EXECBUFFER: u64 = drm_ioctl_t::<DrmVirtgpuExecbuffer>(IOC_RW, 0x42);
pub const DRM_IOCTL_VIRTGPU_GETPARAM: u64 = drm_ioctl_t::<DrmVirtgpuGetparam>(IOC_RW, 0x43);
pub const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE: u64 = drm_ioctl_t::<DrmVirtgpuResourceCreate>(IOC_RW, 0x44);
pub const DRM_IOCTL_VIRTGPU_RESOURCE_INFO: u64 = drm_ioctl_t::<DrmVirtgpuResourceInfo>(IOC_RW, 0x45);
pub const DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST: u64 = drm_ioctl_t::<DrmVirtgpu3dTransfer>(IOC_RW, 0x46);
pub const DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST: u64 = drm_ioctl_t::<DrmVirtgpu3dTransfer>(IOC_RW, 0x47);
pub const DRM_IOCTL_VIRTGPU_WAIT: u64 = drm_ioctl_t::<DrmVirtgpuWait>(IOC_RW, 0x48);
pub const DRM_IOCTL_VIRTGPU_GET_CAPS: u64 = drm_ioctl_t::<DrmVirtgpuGetCaps>(IOC_RW, 0x49);
pub const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB: u64 =
    drm_ioctl_t::<DrmVirtgpuResourceCreateBlob>(IOC_RW, 0x4A);
pub const DRM_IOCTL_VIRTGPU_CONTEXT_INIT: u64 = drm_ioctl_t::<DrmVirtgpuContextInit>(IOC_RW, 0x4B);

/// and the client fills them with a pointer and 0x10, so they are certainly not Pin each number to the value a userspace build gets from `<drm/*.h>`.
/// and the client fills them with a pointer and 0x10, so they are certainly not
/// and the client fills them with a pointer and 0x10, so they are certainly not The reference values are the macros as expanded by libdrm 26.2 (the
/// and the client fills them with a pointer and 0x10, so they are certainly not headers a guest Mesa is compiled against). If a struct layout or a
/// and the client fills them with a pointer and 0x10, so they are certainly not direction/nr above is ever edited, this fails to compile with the name of
/// and the client fills them with a pointer and 0x10, so they are certainly not the offender instead of shipping an ioctl userspace cannot reach.
macro_rules! abi_pin {
    ($($name:ident = $value:expr;)*) => {
        $(
            const _: () = assert!(
                $name == $value,
                concat!(
                    "DRM ABI drift: ", stringify!($name),
                    " no longer matches the number userspace sends \
                     (libdrm <drm/*.h>). Either the struct layout drifted or \
                     the request number is wrong; both are wire-visible.",
                )
            );
        )*
    };
}

abi_pin! {
    DRM_IOCTL_VERSION = 0xC0406400;
    DRM_IOCTL_GET_CAP = 0xC010640C;
    DRM_IOCTL_SET_CLIENT_CAP = 0x4010640D;
    DRM_IOCTL_GEM_CLOSE = 0x40086409;
    DRM_IOCTL_AUTH_MAGIC = 0x40046411;
    DRM_IOCTL_SET_MASTER = 0x0000641E;
    DRM_IOCTL_DROP_MASTER = 0x0000641F;
    DRM_IOCTL_MODE_GETRESOURCES = 0xC04064A0;
    DRM_IOCTL_MODE_GETCRTC = 0xC06864A1;
    DRM_IOCTL_MODE_SETCRTC = 0xC06864A2;
    DRM_IOCTL_MODE_CURSOR = 0xC01C64A3;
    DRM_IOCTL_MODE_GETGAMMA = 0xC02064A4;
    DRM_IOCTL_MODE_SETGAMMA = 0xC02064A5;
    DRM_IOCTL_MODE_GETENCODER = 0xC01464A6;
    DRM_IOCTL_MODE_GETCONNECTOR = 0xC05064A7;
    DRM_IOCTL_MODE_GETPROPERTY = 0xC04064AA;
    DRM_IOCTL_MODE_GETPROPBLOB = 0xC01064AC;
    DRM_IOCTL_MODE_ADDFB = 0xC01C64AE;
    DRM_IOCTL_MODE_RMFB = 0xC00464AF;
    DRM_IOCTL_MODE_PAGE_FLIP = 0xC01864B0;
    DRM_IOCTL_MODE_DIRTYFB = 0xC01864B1;
    DRM_IOCTL_MODE_CREATE_DUMB = 0xC02064B2;
    DRM_IOCTL_MODE_MAP_DUMB = 0xC01064B3;
    DRM_IOCTL_MODE_DESTROY_DUMB = 0xC00464B4;
    DRM_IOCTL_MODE_GETPLANERESOURCES = 0xC01064B5;
    DRM_IOCTL_MODE_GETPLANE = 0xC02064B6;
    DRM_IOCTL_MODE_SETPLANE = 0xC03064B7;
    DRM_IOCTL_MODE_ADDFB2 = 0xC06864B8;
    DRM_IOCTL_MODE_OBJ_GETPROPERTIES = 0xC02064B9;
    DRM_IOCTL_MODE_ATOMIC = 0xC03864BC;
    DRM_IOCTL_PRIME_HANDLE_TO_FD = 0xC00C642D;
    DRM_IOCTL_PRIME_FD_TO_HANDLE = 0xC00C642E;
    DRM_IOCTL_SYNCOBJ_CREATE = 0xC00864BF;
    DRM_IOCTL_SYNCOBJ_DESTROY = 0xC00864C0;
    DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD = 0xC01864C1;
    DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE = 0xC01864C2;
    DRM_IOCTL_SYNCOBJ_WAIT = 0xC02864C3;
    DRM_IOCTL_MODE_CREATEPROPBLOB = 0xC01064BD;
    DRM_IOCTL_MODE_DESTROYPROPBLOB = 0xC00464BE;
    DRM_IOCTL_SYNCOBJ_RESET = 0xC01064C4;
    DRM_IOCTL_SYNCOBJ_SIGNAL = 0xC01064C5;
    DRM_IOCTL_VIRTGPU_MAP = 0xC0106441;
    DRM_IOCTL_VIRTGPU_EXECBUFFER = 0xC0406442;
    DRM_IOCTL_VIRTGPU_GETPARAM = 0xC0106443;
    DRM_IOCTL_VIRTGPU_RESOURCE_CREATE = 0xC0386444;
    DRM_IOCTL_VIRTGPU_RESOURCE_INFO = 0xC0106445;
    DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST = 0xC02C6446;
    DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST = 0xC02C6447;
    DRM_IOCTL_VIRTGPU_WAIT = 0xC0086448;
    DRM_IOCTL_VIRTGPU_GET_CAPS = 0xC0186449;
    DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB = 0xC038644A;
    DRM_IOCTL_VIRTGPU_CONTEXT_INIT = 0xC010644B;
}

pub const VIRTGPU_EXECBUF_FENCE_FD_IN: u32 = 0x01;
pub const VIRTGPU_EXECBUF_FENCE_FD_OUT: u32 = 0x02;
pub const VIRTGPU_EXECBUF_RING_IDX: u32 = 0x04;

/// and the client fills them with a pointer and 0x10, so they are certainly not `VIRGL_BIND_SCANOUT` — the bit that marks a resource as scanout capable.
/// and the client fills them with a pointer and 0x10, so they are certainly not
/// and the client fills them with a pointer and 0x10, so they are certainly not **The `bind` field of `virtio_gpu_ctrl_hdr_3d` is `VIRGL_BIND_*`, not
/// and the client fills them with a pointer and 0x10, so they are certainly not `PIPE_BIND_*`,** despite the virtio spec prose saying otherwise. This is
/// and the client fills them with a pointer and 0x10, so they are certainly not settled empirically: for a gbm scanout buffer Mesa sends `0x14000a`, and
/// and the client fills them with a pointer and 0x10, so they are certainly not decoding it in each namespace gives
/// and the client fills them with a pointer and 0x10, so they are certainly not
/// and the client fills them with a pointer and 0x10, so they are certainly not ```text
/// and the client fills them with a pointer and 0x10, so they are certainly not   VIRGL_BIND_*: RENDER_TARGET | SAMPLER_VIEW | SCANOUT | SHARED   <- coherent
/// and the client fills them with a pointer and 0x10, so they are certainly not   PIPE_BIND_*:  RENDER_TARGET | SAMPLER_VIEW | GLOBAL | QUERY_BUFFER | SHARED
/// and the client fills them with a pointer and 0x10, so they are certainly not ```
/// and the client fills them with a pointer and 0x10, so they are certainly not
/// and the client fills them with a pointer and 0x10, so they are certainly not A texture that is a query buffer is nonsense; a scanout texture that is
/// and the client fills them with a pointer and 0x10, so they are certainly not also `VIRGL_BIND_SCANOUT` is exactly right. So the host interprets this
/// and the client fills them with a pointer and 0x10, so they are certainly not mask as `VIRGL_BIND_*`.
/// and the client fills them with a pointer and 0x10, so they are certainly not
/// and the client fills them with a pointer and 0x10, so they are certainly not The two namespaces disagree, and disagreeing here is silent:
/// and the client fills them with a pointer and 0x10, so they are certainly not `PIPE_BIND_SCANOUT` is `(1 << 19) == 0x80000` in gallium's `p_defines.h`,
/// and the client fills them with a pointer and 0x10, so they are certainly not which in `VIRGL_BIND_*` is `VIRGL_BIND_STAGING` — "backed by guest storage
/// and the client fills them with a pointer and 0x10, so they are certainly not and only read by the host". Setting that on a buffer we want to scan out
/// and the client fills them with a pointer and 0x10, so they are certainly not is precisely backwards.
/// and the client fills them with a pointer and 0x10, so they are certainly not
/// and the client fills them with a pointer and 0x10, so they are certainly not This was `0x200` (`PIPE_BIND_SAMPLER_VIEW_SUBOPTIMAL`), then briefly
/// and the client fills them with a pointer and 0x10, so they are certainly not `0x80000` (`VIRGL_BIND_STAGING`). The correct value is `0x40000`.
pub const VIRGL_BIND_SCANOUT: u32 = 1 << 18;

/// and the client fills them with a pointer and 0x10, so they are certainly not Retained name for readability at the call site. **Not** gallium's
/// and the client fills them with a pointer and 0x10, so they are certainly not `PIPE_BIND_SCANOUT`; see the note on `VIRGL_BIND_SCANOUT` above.
pub use VIRGL_BIND_SCANOUT as PIPE_BIND_SCANOUT;

/// and the client fills them with a pointer and 0x10, so they are certainly not Pin the virgl wire constants to the values the authoritative header defines.
/// and the client fills them with a pointer and 0x10, so they are certainly not
/// and the client fills them with a pointer and 0x10, so they are certainly not Same rationale as `abi_pin!` for the ioctl numbers: these are values shared
/// and the client fills them with a pointer and 0x10, so they are certainly not with Mesa and QEMU, hand-transcribed, and a wrong one does not fail — it
/// and the client fills them with a pointer and 0x10, so they are certainly not presents a frame rather than erroring.
macro_rules! abi_pin_gallium {
    ($($name:ident = $value:expr; $why:expr;)*) => {
        $(
            const _: () = assert!(
                $name == $value,
                concat!(
                    stringify!($name), " no longer matches upstream: ", $why,
                    ". This value is shared with Mesa and QEMU over the wire; \
                     a wrong one presents a frame instead of failing.",
                )
            );
        )*
    };
}

abi_pin_gallium! {
    VIRGL_BIND_SCANOUT = 1 << 18;
    "virgl_hw.h defines VIRGL_BIND_SCANOUT as (1u << 18). (1 << 19) is \
     VIRGL_BIND_STAGING and (1 << 9) has no meaning in this namespace";
}

/// and the client fills them with a pointer and 0x10, so they are certainly not The **complete** set of `VIRGL_RESOURCE_*` flags, from
/// and the client fills them with a pointer and 0x10, so they are certainly not `src/virtio/virtio-gpu/virgl_hw.h`:
/// and the client fills them with a pointer and 0x10, so they are certainly not
/// and the client fills them with a pointer and 0x10, so they are certainly not ```c
/// and the client fills them with a pointer and 0x10, so they are certainly not #define VIRGL_RESOURCE_Y_0_TOP             (1 << 0)
/// and the client fills them with a pointer and 0x10, so they are certainly not #define VIRGL_RESOURCE_FLAG_MAP_PERSISTENT (1 << 1)
/// and the client fills them with a pointer and 0x10, so they are certainly not #define VIRGL_RESOURCE_FLAG_MAP_COHERENT   (1 << 2)
/// and the client fills them with a pointer and 0x10, so they are certainly not ```
/// and the client fills them with a pointer and 0x10, so they are certainly not
/// and the client fills them with a pointer and 0x10, so they are certainly not There are no others. In particular `VIRGL_RESOURCE_NOSCANOUT` and
/// and the client fills them with a pointer and 0x10, so they are certainly not `VIRGL_RESOURCE_SCANOUT_EXTENDED` do not exist, and the kernel previously
/// and the client fills them with a pointer and 0x10, so they are certainly not invented them as 0x02 and 0x04 — which are MAP_PERSISTENT and
/// and the client fills them with a pointer and 0x10, so they are certainly not MAP_COHERENT. So it was telling the host that every scanout candidate was a
/// and the client fills them with a pointer and 0x10, so they are certainly not coherently-mapped resource, and that is not merely a no-op: MAP_COHERENT
/// and the client fills them with a pointer and 0x10, so they are certainly not asserts the host may write without an explicit flush, which is the wrong
/// and the client fills them with a pointer and 0x10, so they are certainly not contract for a texture whose contents are read by the scanout engine.
pub const VIRGL_RESOURCE_Y_0_TOP: u32 = 1 << 0;
pub const VIRGL_RESOURCE_FLAG_MAP_PERSISTENT: u32 = 1 << 1;
pub const VIRGL_RESOURCE_FLAG_MAP_COHERENT: u32 = 1 << 2;

/// and the client fills them with a pointer and 0x10, so they are certainly not Sanity pin: the three flags above must stay mutually exclusive bit
/// and the client fills them with a pointer and 0x10, so they are certainly not positions in a u32, or the "pass flags through untouched" reasoning in
/// and the client fills them with a pointer and 0x10, so they are certainly not `handle_virtgpu_resource_create` no longer holds.
const _: () = assert!(
    VIRGL_RESOURCE_Y_0_TOP == 1
        && VIRGL_RESOURCE_FLAG_MAP_PERSISTENT == 2
        && VIRGL_RESOURCE_FLAG_MAP_COHERENT == 4
);
pub const VIRTGPU_RESOURCE_FLAG_Y_0_TOP: u32 = 1 << 0;

pub const VIRTGPU_PARAM_3D_FEATURES: u64 = 1;
pub const VIRTGPU_PARAM_CAPSET_QUERY_FIX: u64 = 2;
pub const VIRTGPU_PARAM_RESOURCE_BLOB: u64 = 3;
pub const VIRTGPU_PARAM_HOST_VISIBLE: u64 = 4;
pub const VIRTGPU_PARAM_CROSS_DEVICE: u64 = 5;
pub const VIRTGPU_PARAM_CONTEXT_INIT: u64 = 6;
pub const VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS: u64 = 7;
pub const VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME: u64 = 8;
pub const VIRTGPU_PARAM_BLOB_ALIGNMENT: u64 = 9;

// Pin to include/uapi/drm/virtgpu_drm.h:
//   #define VIRTGPU_PARAM_BLOB_ALIGNMENT 9 /* Device alignment requirements for blobs */
// This constant was *missing* here while virtgpu.rs already matched on the
// name. An undefined SCREAMING_CASE identifier in pattern position is not an
// error in Rust -- it silently becomes a *binding* that matches every value no
// earlier arm claimed. The getparam match therefore returned
// `Param::BlobAlignment` for every unknown param and made the `_ =>
// Param::Unknown` arm dead, so a client probing an unsupported feature read
// "present, value 0" instead of EINVAL. `#![allow(unused)]` in virtgpu.rs hid
// the two warnings that would have shown it (unused_variables,
// unreachable_patterns); only the non_snake_case style lint survived, which is
// how it was finally spotted. Pinning the value keeps the name bound to the
// constant instead of a binding.
const _: () = assert!(
    VIRTGPU_PARAM_BLOB_ALIGNMENT == 9,
    "DRM ABI drift: VIRTGPU_PARAM_BLOB_ALIGNMENT != 9 (libdrm <drm/virtgpu_drm.h>)"
);

pub const DRM_FORMAT_XRGB8888: u32 = 0x34325258; // 'XR24'
pub const DRM_FORMAT_ARGB8888: u32 = 0x34325241; // 'AR24'
pub const DRM_FORMAT_XBGR8888: u32 = 0x34324258; // 'XB24'
pub const DRM_FORMAT_ABGR8888: u32 = 0x34324241; // 'AB24'
pub const DRM_FORMAT_RGB565: u32 = 0x36314752;   // 'RG16'

pub const DRM_PLANE_TYPE_OVERLAY: u32 = 0;
pub const DRM_PLANE_TYPE_PRIMARY: u32 = 1;
pub const DRM_PLANE_TYPE_CURSOR: u32 = 2;

pub const DRM_CLIENT_CAP_STEREO_3D: u64 = 1;
pub const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
pub const DRM_CLIENT_CAP_ASPECT_RATIO: u64 = 4;
pub const DRM_CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;

pub const DRM_MODE_CONNECTED: u32 = 1;
pub const DRM_MODE_DISCONNECTED: u32 = 2;

pub const DRM_MODE_ENCODER_NONE: u32 = 0;
pub const DRM_MODE_ENCODER_DAC: u32 = 1;
pub const DRM_MODE_ENCODER_TMDS: u32 = 2;

pub const DRM_CONNECTOR_TYPE_Unknown: u32 = 0;
pub const DRM_CONNECTOR_TYPE_HDMIA: u32 = 11;
pub const DRM_CONNECTOR_TYPE_eDP: u32 = 14;
pub const DRM_CONNECTOR_TYPE_VIRTUAL: u32 = 15;

/// dma-buf.h: `struct dma_buf_sync { __u64 flags; }` — 8 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DmaBufSync {
    pub flags: u64,
}

/// dma-buf.h: `DMA_BUF_SYNC_*` flag bits for `DMA_BUF_IOCTL_SYNC`.
pub const DMA_BUF_SYNC_READ: u32 = 1 << 0;
pub const DMA_BUF_SYNC_WRITE: u32 = 1 << 1;
pub const DMA_BUF_SYNC_RW: u32 = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE;
pub const DMA_BUF_SYNC_START: u32 = 1 << 2;
pub const DMA_BUF_SYNC_END: u32 = 1 << 3;

/// dma-buf.h: `#define DMA_BUF_IOCTL_SYNC _IOWR('b', 0, struct dma_buf_sync)`
/// = `0xC0086200`. This is **not** a DRM ioctl — it uses type `'b'`, so it is
/// computed with `ioc` directly rather than `drm_ioctl_t` (which hardcodes
/// `'d'`).
pub const DMA_BUF_IOCTL_SYNC: u64 = ioc(
    IOC_RW,
    b'b' as u32,
    0x00,
    core::mem::size_of::<DmaBufSync>() as u32,
);

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeCrtcLut {
    pub crtc_id: u32,
    pub gamma_size: u32,
    pub red: u64,
    pub green: u64,
    pub blue: u64,
}

// Size pin: 32 bytes (matches DRM_IOCTL_MODE_GETGAMMA/SETGAMMA payload).
const _: () = assert!(core::mem::size_of::<DrmModeCrtcLut>() == 32);

pub const DRM_CAP_DUMB_BUFFER: u64 = 1;
pub const DRM_CAP_VBLANK_HIGH_CRTC: u64 = 2;
pub const DRM_CAP_DUMB_PREFERRED_DEPTH: u64 = 3;
pub const DRM_CAP_DUMB_PREFER_SHADOW: u64 = 4;
pub const DRM_CAP_PRIME: u64 = 0x5;
pub const DRM_PRIME_CAP_IMPORT: u64 = 0x1;
pub const DRM_PRIME_CAP_EXPORT: u64 = 0x2;
pub const DRM_CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;
pub const DRM_CAP_SYNCOBJ: u64 = 0x13;

pub const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
pub const DRM_MODE_FLAG_PHSYNC: u32 = 1;
pub const DRM_MODE_FLAG_NHSYNC: u32 = 2;
pub const DRM_MODE_FLAG_PVSYNC: u32 = 4;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmVersion {
    pub version_major: i32,
    pub version_minor: i32,
    pub version_patchlevel: i32,
    pub name_len: usize,
    pub name: *mut i8,
    pub date_len: usize,
    pub date: *mut i8,
    pub desc_len: usize,
    pub desc: *mut i8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmGetCap {
    pub capability: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmSetClientCap {
    pub capability: u64,
    pub value: u64,
}

pub const DRM_MODE_PROP_PENDING: u32 = 1 << 0;
pub const DRM_MODE_PROP_RANGE: u32 = 1 << 1;
pub const DRM_MODE_PROP_IMMUTABLE: u32 = 1 << 2;
pub const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
pub const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
pub const DRM_MODE_PROP_BITMASK: u32 = 1 << 5;
pub const DRM_MODE_PROP_OBJECT: u32 = 1 << 6;
pub const DRM_MODE_PROP_SIGNED_RANGE: u32 = 2 << 6;
pub const DRM_MODE_PROP_ATOMIC: u32 = 0x80000000;

pub const DRM_MODE_OBJECT_CRTC: u32 = 0xcccccccc;
pub const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0c0c0;
pub const DRM_MODE_OBJECT_ENCODER: u32 = 0xe0e0e0e0;
pub const DRM_MODE_OBJECT_MODE: u32 = 0xdededede;
pub const DRM_MODE_OBJECT_PROPERTY: u32 = 0xb0b0b0b0;
pub const DRM_MODE_OBJECT_FB: u32 = 0xfbfbfbfb;
pub const DRM_MODE_OBJECT_BLOB: u32 = 0xbbbbbbbb;
pub const DRM_MODE_OBJECT_PLANE: u32 = 0xeeeeeeee;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModePropertyEnum {
    pub value: u64,
    pub name: [u8; 32],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeObjGetProperties {
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_props: u32,
    pub obj_id: u32,
    pub obj_type: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeGetProperty {
    pub values_ptr: u64,
    pub enum_blob_ptr: u64,
    pub prop_id: u32,
    pub flags: u32,
    pub name: [u8; 32],
    pub count_values: u32,
    pub count_enum_blobs: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeGetPropBlob {
    pub blob_id: u32,
    pub length: u32,
    pub data: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub type_: u32,
    pub name: [u8; 32],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeCardRes {
    pub fb_id_ptr: u64,
    pub crtc_id_ptr: u64,
    pub connector_id_ptr: u64,
    pub encoder_id_ptr: u64,
    pub count_fbs: u32,
    pub count_crtcs: u32,
    pub count_connectors: u32,
    pub count_encoders: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeCrtc {
    pub set_connectors_ptr: u64,
    pub count_connectors: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: DrmModeInfo,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeSetCrtc {
    pub set_connectors_ptr: u64,
    pub count_connectors: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: DrmModeInfo,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeGetConnector {
    pub encoders_ptr: u64,
    pub modes_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_modes: u32,
    pub count_props: u32,
    pub count_encoders: u32,
    pub encoder_id: u32,
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeFbCmd {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub depth: u32,
    pub handle: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeFbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub modifier: [u64; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModePageFlip {
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub reserved: u32,
    pub user_data: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeFbDirtyCmd {
    pub fb_id: u32,
    pub flags: u32,
    pub color: u32,
    pub num_clips: u32,
    pub clips_ptr: u64,
}

/// and the client fills them with a pointer and 0x10, so they are certainly not drm_clip_rect — wire format for DIRTYFB damage clips (matches Linux UAPI).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmClipRect {
    pub x1: u16,
    pub y1: u16,
    pub x2: u16,
    pub y2: u16,
}

/// and the client fills them with a pointer and 0x10, so they are certainly not DIRTYFB flags: discard pending clips, transfer full frame (damage unknown,
// e.g. fresh buffer or evicted damage history). num_clips==0 with no flags
// is a no-op.
/// NOT an upstream name. Upstream `include/uapi/drm/drm_mode.h` defines:
///
/// ```c
/// #define DRM_MODE_FB_DIRTY_ANNOTATE_COPY 0x01
/// #define DRM_MODE_FB_DIRTY_FLAGS         0x03
/// #define DRM_MODE_FB_DIRTY_MAX_CLIPS     256
/// ```
///
/// So bit 0 of `drm_mode_fb_dirty_cmd.flags` is **ANNOTATE_COPY**, meaning "the
/// client has already damaged these regions itself, copy the framebuffer
/// elsewhere without redrawing". It does NOT mean "transfer everything".
///
/// This constant used to be `0x1` and was tested as if it meant "full update",
/// so any client setting ANNOTATE_COPY had its clip list discarded and the
/// whole framebuffer re-uploaded. The value is unchanged (it is the same bit);
/// what changed is that nothing may read bit 0 as "full" any more.
///
/// Our internal "no clip information, do a full transfer" decision is expressed
/// with `num_clips == 0`, which is what `handle_dirtyfb` now tests.
pub const DRM_FB_DIRTY_ANNOTATE_COPY: u32 = 0x01;
/// Mask of the defined flag bits. Any bit outside this is undefined and must be
/// masked off before testing, as upstream does.
pub const DRM_MODE_FB_DIRTY_FLAGS: u32 = 0x03;
/// Upstream's clip cap. We previously used 64, so a legal 65..=256 clip request
/// was silently downgraded to a full-frame transfer.
pub const DRM_MODE_FB_DIRTY_MAX_CLIPS: u32 = 256;

/// and the client fills them with a pointer and 0x10, so they are certainly not drm_mode_cursor — DRM_IOCTL_MODE_CURSOR payload.
pub const DRM_MODE_CURSOR_MOVE: u32 = 1;
pub const DRM_MODE_CURSOR_BO: u32 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeCursor {
    pub flags: u32,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub handle: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeCreateDumb {
    pub height: u32,
    pub width: u32,
    pub bpp: u32,
    pub flags: u32,
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeMapDumb {
    pub handle: u32,
    pub pad: u32,
    pub offset: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmModeDestroyDumb {
    pub handle: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmPrimeHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeGetPlaneRes {
    pub plane_id_ptr: u64,
    pub count_planes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeGetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
    pub count_format_types: u32,
    pub format_type_ptr: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeSetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_h: u32,
    pub src_w: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuGetparam {
    pub param: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuGetCaps {
    pub name: u32,
    /// and the client fills them with a pointer and 0x10, so they are certainly not Linux ABI: capset_version (Mesa virgl passes 1, venus passes its version).
    pub capset_version: u32,
    pub addr: u64,
    pub size: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuMap {
    pub offset: u64,
    pub handle: u32,
    pub pad: u32,
}

/// virtgpu_drm.h:64-68 — one entry of the in/out syncobj arrays referenced by
/// `drm_virtgpu_execbuffer`.
///
/// This was absent entirely, which is why `handle_virtgpu_execbuffer` could
/// not parse the syncobj tail: it read `num_out_syncobjs` only as a boolean
/// for fence allocation and otherwise ignored both arrays.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuExecbufferSyncobj {
    pub handle: u32,
    pub flags: u32,
    pub point: u64,
}

// virtgpu_drm.h:60-63
pub const VIRTGPU_EXECBUF_SYNCOBJ_RESET: u32 = 0x01;
pub const VIRTGPU_EXECBUF_SYNCOBJ_FLAGS: u32 = VIRTGPU_EXECBUF_SYNCOBJ_RESET;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuExecbuffer {
    pub flags: u32,
    pub size: u32,
    pub command: u64,
    pub bo_handles: u64,
    pub num_bo_handles: u32,
    /// and the client fills them with a pointer and 0x10, so they are certainly not In/out: userspace passes a fence to wait on, the kernel writes back a
    /// and the client fills them with a pointer and 0x10, so they are certainly not fence to wait on when VIRTGPU_EXECBUF_FENCE_FD_OUT is requested.
    pub fence_fd: i32,
    /// and the client fills them with a pointer and 0x10, so they are certainly not Venus-only command ring index; virgl leaves it zero.
    pub ring_idx: u32,
    pub syncobj_stride: u32,
    pub num_in_syncobjs: u32,
    pub num_out_syncobjs: u32,
    pub in_syncobjs: u64,
    pub out_syncobjs: u64,
    // NOTE: there is deliberately no `pad: u32` between `ring_idx` and
    // `syncobj_stride`. Linux's drm_virtgpu_execbuffer has no such field, so
    // adding one here shifts the whole syncobj tail 4 bytes forward and makes
    // this struct 72 bytes instead of 64. Every Venus/virgl submit then
    // reads `num_out_syncobjs` from the middle of the out_syncobjs pointer,
    // reads in/out syncobj arrays swapped, and dereferences a `point` field
    // 8 bytes past the end of the client's 64-byte payload. The `abi_pin!`
    // assertion on DRM_IOCTL_VIRTGPU_EXECBUFFER below fails to compile if
    // that ever happens again.
}

/// and the client fills them with a pointer and 0x10, so they are certainly not Reply to DRM_IOCTL_VIRTGPU_RESOURCE_INFO: the host-side resource backing a
/// and the client fills them with a pointer and 0x10, so they are certainly not buffer object, as virgl uses to look up a `virgl_renderer_resource`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuResourceInfo {
    pub bo_handle: u32,
    pub res_handle: u32,
    pub size: u32,
    pub blob_mem: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuResourceCreate {
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
    pub bo_handle: u32,
    pub res_handle: u32,
    pub size: u32,
    pub stride: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuContextCreate {
    pub nlen: u32,
    pub context_init: u32,
    pub debug_name: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuContextDestroy {
    pub context_id: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuWait {
    pub handle: u32,
    pub flags: u32,
}

// ponytail: Venus Vulkan path (layouts from libdrm virtgpu_drm.h).
pub const VIRTGPU_BLOB_MEM_GUEST: u32 = 0x0001;
pub const VIRTGPU_BLOB_MEM_HOST3D: u32 = 0x0002;
// virtgpu_drm.h:184 — host-visible blob with guest-resident staging. Was
// missing here, so the blob_mem check in virtgpu.rs rejected 0x0003 with
// EINVAL where upstream accepts it (virtgpu_ioctl.c:504-516 treats it as
// guest + host3d). A client using it took a different mem model as fallback
// rather than reporting the error.
pub const VIRTGPU_BLOB_MEM_HOST3D_GUEST: u32 = 0x0003;
pub const VIRTGPU_BLOB_FLAG_USE_MAPPABLE: u32 = 0x0001;
pub const VIRTGPU_BLOB_FLAG_USE_SHAREABLE: u32 = 0x0002;
pub const VIRTGPU_BLOB_FLAG_USE_CROSS_DEVICE: u32 = 0x0004;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuResourceCreateBlob {
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub bo_handle: u32,
    pub res_handle: u32,
    pub size: u64,
    pub pad: u32,
    pub cmd_size: u32,
    pub cmd: u64,
    pub blob_id: u64,
    pub blob_hints: u32,
    pub pad2: u32,
}

pub const VIRTGPU_CONTEXT_PARAM_CAPSET_ID: u64 = 0x0001;
pub const VIRTGPU_CONTEXT_PARAM_NUM_RINGS: u64 = 0x0002;
pub const VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK: u64 = 0x0003;
pub const VIRTGPU_CONTEXT_PARAM_DEBUG_NAME: u64 = 0x0004;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuContextSetParam {
    pub param: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpuContextInit {
    pub num_params: u32,
    pub pad: u32,
    pub ctx_set_params: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmSyncobjCreate {
    pub handle: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmSyncobjDestroy {
    pub handle: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmSyncobjHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
    pub pad: u32,
    pub point: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmGemClose {
    pub handle: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmSyncobjWait {
    pub handles: u64,
    pub timeout_nsec: i64,
    pub count_handles: u32,
    pub flags: u32,
    pub first_signaled: u32,
    pub pad: u32,
    pub deadline_nsec: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeCreateBlob {
    pub data: u64,
    pub length: u32,
    pub blob_id: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeDestroyBlob {
    pub blob_id: u32,
}

pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
pub const DRM_MODE_PAGE_FLIP_ASYNC: u32 = 0x02;
pub const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;
pub const DRM_MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
pub const DRM_MODE_ATOMIC_ALLOW_MODESET: u32 = 0x0400;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmModeAtomic {
    pub flags: u32,
    pub count_objs: u32,
    pub objs_ptr: u64,
    pub count_props_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub reserved: u64,
    pub user_data: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrmVirtgpu3dBox {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmVirtgpu3dTransfer {
    pub bo_handle: u32,
    pub box_: DrmVirtgpu3dBox,
    pub level: u32,
    pub offset: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmSyncobjArray {
    pub handles: u64,
    pub count_handles: u32,
    pub pad: u32,
}

// The SYNCOBJ_* numbers are declared with the rest of the ioctl numbers at the
// top of this file: each is `_IOWR('d', nr, size_of::<the struct above>())`, so
// the encoded size always matches the payload the kernel actually reads.


// ── Regression tests that actually run ───────────────────────────────────────
//
// The virtgpu errno/constant logic also has unit tests in
// `crates/kernel/src/drivers/drm/virtgpu_ioctl.rs`, but a `no_std` kernel that
// defines `#[panic_handler]` cannot be built by `cargo test` at all (duplicate
// `panic_impl` against the harness's `std`). That module therefore never
// compiled, never ran, and its guards protected nothing -- which is how a
// getparam bug that returns success for unknown params, and execbuf flag
// constants whose bits were rotated by one, both survived.
//
// These live in a plain std library so `cargo test -p drm-uapi` enforces them.

#[cfg(test)]
mod abi_regression_tests {
    use super::*;

    /// virtgpu_drm.h:52-54 — the exact three bits.
    ///
    /// A local restatement of these was found with FENCE_FD_OUT and RING_IDX
    /// swapped relative to upstream (RING_IDX=0x02, FENCE_FD_OUT=0x01), so
    /// `needs_fence` tested bit 0x01 — FENCE_FD_IN — and the kernel ended up
    /// `eventfd_signal_fd()`-ing the client's *input* fence.
    #[test]
    fn execbuf_flag_bits_are_upstream_values() {
        assert_eq!(VIRTGPU_EXECBUF_FENCE_FD_IN, 0x01);
        assert_eq!(VIRTGPU_EXECBUF_FENCE_FD_OUT, 0x02);
        assert_eq!(VIRTGPU_EXECBUF_RING_IDX, 0x04);
    }

    /// The bits must be distinct: an alias here is exactly how the swap above
    /// went unnoticed, since `needs_fence` still "worked" for some inputs.
    #[test]
    fn execbuf_flag_bits_are_distinct() {
        let bits = [
            VIRTGPU_EXECBUF_FENCE_FD_IN,
            VIRTGPU_EXECBUF_FENCE_FD_OUT,
            VIRTGPU_EXECBUF_RING_IDX,
        ];
        for (i, a) in bits.iter().enumerate() {
            for (j, b) in bits.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "execbuf flag bits {} and {} collide", i, j);
                }
            }
        }
    }

    /// Outgoing flags must include all three defined bits; this is the mask
    /// `flags_are_known` tests against, so a missing bit would make the driver
    /// reject a legitimate client submission.
    #[test]
    fn outgoing_flag_mask_covers_every_defined_bit() {
        let mask = VIRTGPU_EXECBUF_FENCE_FD_IN
            | VIRTGPU_EXECBUF_FENCE_FD_OUT
            | VIRTGPU_EXECBUF_RING_IDX;
        for bit in [
            VIRTGPU_EXECBUF_FENCE_FD_IN,
            VIRTGPU_EXECBUF_FENCE_FD_OUT,
            VIRTGPU_EXECBUF_RING_IDX,
        ] {
            assert_eq!(mask & bit, bit, "mask 0x{mask:x} is missing bit 0x{bit:x}");
        }
        assert_eq!(mask, 0x07);
    }

    /// virtgpu_drm.h:104 — VIRTGPU_PARAM_BLOB_ALIGNMENT is 9.
    ///
    /// This constant was *absent* while `virtgpu.rs` already matched on the
    /// name. An undefined SCREAMING_CASE identifier in pattern position is not
    /// an error in Rust: it silently binds, matching every value no earlier arm
    /// claimed. Every unknown getparam therefore returned the blob-alignment
    /// answer instead of EINVAL, and the `_ => Unknown` arm became dead code.
    #[test]
    fn virtgpu_params_are_the_upstream_sequence() {
        assert_eq!(VIRTGPU_PARAM_3D_FEATURES, 1);
        assert_eq!(VIRTGPU_PARAM_CAPSET_QUERY_FIX, 2);
        assert_eq!(VIRTGPU_PARAM_RESOURCE_BLOB, 3);
        assert_eq!(VIRTGPU_PARAM_HOST_VISIBLE, 4);
        assert_eq!(VIRTGPU_PARAM_CROSS_DEVICE, 5);
        assert_eq!(VIRTGPU_PARAM_CONTEXT_INIT, 6);
        assert_eq!(VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS, 7);
        assert_eq!(VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME, 8);
        assert_eq!(VIRTGPU_PARAM_BLOB_ALIGNMENT, 9);
    }

    /// Every param constant must be unique, so no two params can alias into one
    /// match arm (the failure mode of a wrong or missing value).
    #[test]
    fn virtgpu_param_values_are_unique() {
        let params = [
            VIRTGPU_PARAM_3D_FEATURES,
            VIRTGPU_PARAM_CAPSET_QUERY_FIX,
            VIRTGPU_PARAM_RESOURCE_BLOB,
            VIRTGPU_PARAM_HOST_VISIBLE,
            VIRTGPU_PARAM_CROSS_DEVICE,
            VIRTGPU_PARAM_CONTEXT_INIT,
            VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS,
            VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME,
            VIRTGPU_PARAM_BLOB_ALIGNMENT,
        ];
        for (i, a) in params.iter().enumerate() {
            for (j, b) in params.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "param {} and {} alias to {}", i, j, a);
                }
            }
        }
    }

    /// Blob mem-type values (virtgpu_drm.h:182-184).
    #[test]
    fn blob_mem_types_are_upstream_values() {
        assert_eq!(VIRTGPU_BLOB_MEM_GUEST, 0x0001);
        assert_eq!(VIRTGPU_BLOB_MEM_HOST3D, 0x0002);
        assert_eq!(VIRTGPU_BLOB_MEM_HOST3D_GUEST, 0x0003);
    }

    /// The execbuffer payload is 64 bytes upstream. This is the size encoded
    /// into the ioctl number, so a layout change here is wire-visible and the
    /// abi_pin! above must fail rather than let it drift.
    #[test]
    fn execbuffer_is_64_bytes() {
        assert_eq!(core::mem::size_of::<DrmVirtgpuExecbuffer>(), 64);
    }

    /// virtgpu_drm.h:64-68 — one syncobj entry is 16 bytes: u32 handle,
    /// u32 flags, u64 point. `point` needs 8-byte alignment, so it must sit
    /// at offset 8; a narrower type or a reordered field would silently
    /// misparse every client-supplied dependency.
    #[test]
    fn execbuffer_syncobj_entry_layout_is_upstream() {
        assert_eq!(core::mem::size_of::<DrmVirtgpuExecbufferSyncobj>(), 16);
        assert_eq!(core::mem::align_of::<DrmVirtgpuExecbufferSyncobj>(), 8);
        let e = DrmVirtgpuExecbufferSyncobj::default();
        let base = &e as *const _ as usize;
        assert_eq!(&e.handle as *const _ as usize - base, 0);
        assert_eq!(&e.flags as *const _ as usize - base, 4);
        assert_eq!(&e.point as *const _ as usize - base, 8);
    }

    /// virtgpu_drm.h:60-63 — the only defined syncobj flag is RESET.
    #[test]
    fn execbuf_syncobj_flag_is_upstream_value() {
        assert_eq!(VIRTGPU_EXECBUF_SYNCOBJ_RESET, 0x01);
        assert_eq!(VIRTGPU_EXECBUF_SYNCOBJ_FLAGS, 0x01);
    }

    /// drm_mode.h: struct drm_mode_atomic is exactly 56 bytes.
    #[test]
    fn drm_mode_atomic_is_56_bytes() {
        assert_eq!(core::mem::size_of::<DrmModeAtomic>(), 56);
        assert_eq!(core::mem::align_of::<DrmModeAtomic>(), 8);
        assert_eq!(DRM_IOCTL_MODE_ATOMIC, 0xC03864BC);
    }

    /// dma-buf.h: struct dma_buf_sync is exactly 8 bytes (u64 flags).
    #[test]
    fn dma_buf_sync_is_8_bytes() {
        assert_eq!(core::mem::size_of::<DmaBufSync>(), 8);
        assert_eq!(core::mem::align_of::<DmaBufSync>(), 8);
    }

    /// dma-buf.h: DMA_BUF_IOCTL_SYNC = _IOWR('b', 0, struct dma_buf_sync).
    #[test]
    fn dma_buf_sync_ioctl_number_is_upstream() {
        assert_eq!(DMA_BUF_IOCTL_SYNC, 0xC0086200);
        assert_eq!(DMA_BUF_SYNC_RW, DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE);
    }
}
