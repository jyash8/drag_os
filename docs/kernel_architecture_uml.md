# DragOS Kernel Architecture & UML Diagrams

This document outlines the core architecture and object relationships of the **DragOS** microkernel/hybrid kernel codebase. DragOS is a 64-bit multi-core (`x86_64`) operating system written in Rust (`no_std`), targeting modern hardware (APIC, SMP, ACPI, VirtIO, PCI, DRM/KMS, USB xHCI) with a POSIX/Linux-compatible system call interface.

---

## 1. High-Level Subsystem Architecture

The kernel is structured into modular layers spanning architecture support, memory management, multiprocessing/scheduling, virtual filesystem, driver frameworks, and POSIX syscall emulation:

```mermaid
graph TB
    subgraph Userspace ["Userspace Layer"]
        App["POSIX Binaries / Musl / Glibc"]
        Compositor["mywima (Wayland Compositor)"]
        Seatd["seatd (Session Manager)"]
        Foot["foot (Wayland Terminal)"]
    end

    subgraph SyscallBoundary ["System Call & Trap Boundary"]
        SyscallEntry["syscall_entry (IA32_LSTAR / MSR)"]
        IDT["IDT Gates (Page Fault, Timer, Yield int 0x81)"]
    end

    subgraph ProcessMgmt ["Process, Thread & Scheduler Subsystem"]
        Tasks["tasks.rs (Process / Thread / Signal)"]
        CFS["sched.rs (CFS SchedRunQueue / vruntime)"]
        SMP["smp.rs (Per-Core CPUS[k] / Idle / APIC)"]
    end

    subgraph MemoryMgmt ["Memory Subsystem"]
        FA["FRAME_ALLOCATOR (Buddy / Bitmap)"]
        Heap["Kernel Heap (16 MiB linked_list_allocator)"]
        VM["space.rs (PML4 Mapper, COW Table, VMAs)"]
        DMA["dma.rs (SgTable, Coherent DMA, GEM)"]
    end

    subgraph StorageVFS ["VFS & Filesystem Subsystem"]
        VFS["vfs.rs (VNode, FileNode, Pipes, PTY)"]
        Sysfs["sysfs.rs (/sys Device Registry)"]
        Ext4["fs/ext4 & fs/ext2"]
        TarInitrd["fs/tar (Initrd Fallback)"]
    end

    subgraph DriversSubsystem ["Drivers & Hardware Abstraction"]
        DRM["DRM/KMS Core (CRTC, Encoders, Connectors, Planes)"]
        VirtIOGpu["VirtIO-GPU (2D / Scanout / VirGL)"]
        Input["Input / Evdev Table (PS/2, xHCI HID, Uevents)"]
        VirtIONet["virtio/net.rs & net/ (smoltcp stack)"]
        VirtIOBlk["virtio/blk.rs & ahci.rs / nvme.rs"]
    end

    %% Flow connections
    App --> SyscallEntry
    Compositor --> SyscallEntry
    Seatd --> SyscallEntry
    Foot --> SyscallEntry

    SyscallEntry --> Tasks
    IDT --> Tasks
    Tasks --> CFS
    CFS --> SMP

    Tasks --> VM
    VM --> FA
    Tasks --> VFS

    VFS --> Ext4
    VFS --> TarInitrd
    VFS --> Sysfs
    VFS --> DRM
    VFS --> Input

    DRM --> VirtIOGpu
    VFS --> VirtIOBlk
    VFS --> VirtIONet
```

---

## 2. Core Kernel Class / Struct UML Diagram

Below is the UML class diagram representing the primary data structures, their internal fields, and ownership/referential relationships (`Arc`, `SpinLock`, compositions):

```mermaid
classDiagram
    direction TB

    class CpuLocal {
        +u32 id
        +TaskStateSegment tss
        +Gdt gdt
        +Option~Thread~ current
        +u64 idle_stack_top
        +AtomicBool active
    }

    class Thread {
        +u64 id
        +Arc~SpinLock~Process~~ process
        +u64 fs_base
        +u64 stack_ptr
        +u64 stack_limit
        +u64 kernel_stack_top
        +u64 syscall_stack_top
        +u64 sigmask
        +u64 pending
        +VecDeque~SigInfo~ sigqueue
        +u8 priority
        +i32 nice
        +u64 vruntime
        +Option~PendingBlock~ pending_block
        +Arc~FpuBuf~ fpu
        +Arc~FpuBuf~ uc_fpu
    }

    class Process {
        +u64 pid
        +u64 ppid
        +PhysAddr cr3
        +VirtAddr heap_brk
        +VirtAddr mmap_base
        +VmaList vmas
        +Vec~Option~Arc~SpinLock~OpenFile~~~~ fd_table
        +ProcessStatus status
        +i32 exit_code
        +Vec~Arc~SpinLock~Process~~~ children
        +SigAction[32] sigactions
        +u64 pending
        +VecDeque~SigInfo~ sigqueue
        +String cwd
        +usize threads_count
        +u64 utime
        +u64 stime
        +RLimit[16] rlimits
        +fork_from(child_pid, child_cr3) Process
    }

    class OpenFile {
        +VNode vnode
        +u32 flags
        +String path
    }

    class VNode {
        <<enumeration>>
        InitrdFile
        BlockFile
        FsNode(Arc~FileNode~)
        Directory
        Pipe(Arc~SpinLock~PipeBuffer~~)
        TtyDevice
        PtyMaster(Arc~SpinLock~PtyPair~~)
        PtySlave(Arc~SpinLock~PtyPair~~)
        InputEventDevice(usize minor)
        DrmCard
        DrmRender
        FbDevice
        UnixSocket
        InetSocket
    }

    class SchedRunQueue {
        +BinaryHeap~RunQueueEntry~ heap
        +u64 min_vruntime
        +enqueue(Thread)
        +enqueue_with_boost(Thread, u64)
        +pick_next() Option~Thread~
    }

    class RunQueueEntry {
        +Thread thread
        +u64 vruntime
    }

    class DrmDevice {
        +Vec~DrmCrtcState~ crtcs
        +Vec~DrmConnectorState~ connectors
        +Vec~DrmEncoderState~ encoders
        +BTreeMap~u32, DrmFramebuffer~ framebuffers
        +BTreeMap~u32, DumbBuffer~ dumb_buffers
        +u32 next_id
        +alloc_id() u32
    }

    class InputDev {
        +BTreeMap~usize, VecDeque~InputEvent~~ readers
        +u8[96] key_state
        +i32[8] abs_vals
        +u8 led_state
        +Option~usize~ grabbed_by
    }

    CpuLocal "1" o-- "0..1" Thread : current
    Thread "1" o-- "1" Process : shares via Arc
    Process "1" *-- "0..*" OpenFile : owns fd_table
    OpenFile "1" *-- "1" VNode : wraps
    SchedRunQueue "1" *-- "0..*" RunQueueEntry : prioritizes
    RunQueueEntry "1" *-- "1" Thread : references
    VNode ..> InputDev : indexes by minor
    VNode ..> DrmDevice : manages
```

---

## 3. Thread State & Scheduling Lifecycle

State transitions of a `Thread` managed by the Completely Fair Scheduler (CFS) and per-core APIC timer interruptions:

```mermaid
stateDiagram-v2
    [*] --> Runnable : Thread creation or fork

    state Runnable {
        [*] --> InHeap : enqueue to runqueue
        InHeap --> Dispatched : pick_next on timer tick
    }

    Dispatched --> Running : iretq to user mode
    
    state Running {
        ExecuteUser : User-mode Execution Ring 3
        SyscallKernel : Syscall Handler Ring 0 IF=0
        ExecuteUser --> SyscallKernel : syscall or trap
        SyscallKernel --> ExecuteUser : sysretq or iretq
    }

    Running --> Runnable : Timer tick preemption
    Running --> Blocked : block_current_thread
    
    state Blocked {
        KeyboardWait : KEYBOARD_WAIT_QUEUE
        ChildWait : CHILD_WAIT_QUEUE
        PipeWait : PIPE_WAIT_QUEUE
        FutexWait : FUTEX_WAIT_QUEUE
        InputWait : INPUT_WAIT_QUEUE
    }

    Blocked --> Runnable : wake_waiters with boost
    Running --> Zombie : exit_current_thread
    Zombie --> [*] : wait4 reaped by parent
```

---

## 4. Input & KMS Graphics Event Flow

Flow of hardware interrupts from input devices and GPU frame completion into userspace compositors and terminals:

```mermaid
sequenceDiagram
    autonumber
    actor User as User Hardware
    participant KbdMouse as PS/2 / USB xHCI Controller
    participant Driver as keyboard.rs / xhci.rs
    participant Evdev as InputDev Table (/dev/input/eventX)
    participant Netlink as Uevent Socket
    participant Compositor as mywima / libinput
    participant KMS as DRM/KMS (VirtIO-GPU / GOP)

    User->>KbdMouse: Keystroke or Mouse Movement
    KbdMouse->>Driver: Hardware IRQ (IRQ1, IRQ12, or MSI-X)
    Driver->>Evdev: input_push(minor, InputEvent)
    Note over Evdev: Updates shadow state (key_state, abs_vals)
    Evdev->>Compositor: Poll/Read event delivery (via OpenFile FIFO)
    
    rect rgb(240, 248, 255)
        Note over Compositor,KMS: Rendering & Page Flip
        Compositor->>KMS: DRM_IOCTL_MODE_PAGE_FLIP / SETCRTC
        KMS->>KMS: validate mode & scanout surface
        KMS-->>Compositor: DRM_EVENT_FLIP_COMPLETE
    end

    opt Device Hotplug
        User->>Driver: Plug USB Device / Display Hotplug
        Driver->>Netlink: uevent_broadcast("add" / "change")
        Netlink->>Compositor: Netlink Datagram Received
        Compositor->>Evdev: Rescan & Open new /dev/input/eventN
    end
```

---

## 5. Summary of Key Invariants

1. **Lock Hierarchy**: `SCHEDULER` $\to$ `CPUS[k]` $\to$ wait queues $\to$ `process.lock()`.
2. **Interrupt Model**: Spinlocks preserve and restore `RFLAGS.IF`. Non-preemptible syscall Rust dispatcher runs with `IF=0`.
3. **Execution Trail**: Clean, deterministic transition from `kernel_main` to the first user-mode `iretq`.
