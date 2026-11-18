//! Android Hardware Buffer mapping and private Venus worker connections.
use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex, mpsc};

use appine_kernel::errno::Errno;
use appine_kernel::gpu::{Context, Mapping, Renderer};
use jni::objects::{GlobalRef, JClass, JValue};
use jni::{JNIEnv, JavaVM};

unsafe extern "C" {
    fn appine_gpu_capset(caps: *mut u8) -> usize;
    fn appine_venus_worker(fd: i32);
    fn appine_gpu_open(fd: i32, probe: i32) -> i32;
    fn appine_gpu_submit(fd: i32, ring: u32, cmd: *const u8, len: usize, fence: *mut i32) -> i32;
    fn appine_gpu_create(
        fd: i32,
        id: u32,
        blob: u64,
        size: u64,
        kind: *mut i32,
        uuid: *mut u8,
        index: *mut u32,
        allocation: *mut u64,
        handle: *mut i32,
    ) -> i32;
    fn appine_gpu_release(fd: i32, resource: u32);
    fn appine_mapper_create() -> *mut c_void;
    fn appine_mapper_destroy(mapper: *mut c_void);
    fn appine_gpu_map(
        mapper: *mut c_void,
        fd: i32,
        blob: u64,
        size: u64,
        kind: i32,
        uuid: *const u8,
        index: u32,
        allocation: u64,
        memory: *mut u64,
    ) -> *mut u8;
    fn appine_gpu_unmap(mapper: *mut c_void, memory: u64, base: *mut u8, size: usize);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_GpuService_runWorker(
    _: JNIEnv,
    _: JClass,
    fd: i32,
) {
    // The Binder transaction transfers ownership of this fd to the sole worker.
    guarded!("GpuService.runWorker", (), {
        unsafe { appine_venus_worker(fd) }
    })
}

struct Service {
    vm: JavaVM,
    class: GlobalRef,
}

struct Channel {
    fd: OwnedFd,
    service: Arc<Service>,
    connection: GlobalRef,
}

impl Service {
    fn connect(self: &Arc<Self>, probe: bool) -> Result<Channel, Errno> {
        let mut pair = [-1; 2];
        // SAFETY: socketpair initializes both slots on success and transfers ownership.
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        } < 0
        {
            return Err(Errno::EMFILE);
        }
        let [fd, worker] = pair.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
        let timeout = libc::timeval {
            tv_sec: 10,
            tv_usec: 0,
        };
        for opt in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
            if unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    opt,
                    (&raw const timeout).cast(),
                    size_of_val(&timeout) as _,
                )
            } != 0
            {
                return Err(Errno::EIO);
            }
        }
        let mut env = self.vm.attach_current_thread().map_err(|_| Errno::EIO)?;
        let class: &JClass = self.class.as_obj().into();
        let connection = env
            .call_static_method(
                class,
                "connect",
                "(I)Lio/github/mthli/appine/GpuConnection;",
                &[JValue::Int(worker.as_raw_fd())],
            )
            .and_then(|v| v.l())
            .and_then(|v| env.new_global_ref(v));
        drop(worker);
        let connection = connection.map_err(|_| {
            let _ = env.exception_clear();
            Errno::ENODEV
        })?;
        if connection.as_obj().is_null() {
            return Err(Errno::ENODEV);
        }
        let channel = Channel {
            fd,
            connection,
            service: self.clone(),
        };
        if unsafe { appine_gpu_open(channel.fd.as_raw_fd(), i32::from(probe)) } != 0 {
            return Err(Errno::ENODEV);
        }
        Ok(channel)
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        unsafe {
            libc::shutdown(self.fd.as_raw_fd(), libc::SHUT_RDWR);
        }
        if let Ok(mut env) = self.service.vm.attach_current_thread()
            && env
                .call_method(&self.connection, "close", "()V", &[])
                .is_err()
        {
            let _ = env.exception_clear();
        }
    }
}

struct Mapper(NonNull<c_void>);
// The mapper's instance/device and UUID are immutable. Vulkan permits independent
// memory objects to be allocated, mapped and destroyed from different threads.
unsafe impl Send for Mapper {}
unsafe impl Sync for Mapper {}
impl Drop for Mapper {
    fn drop(&mut self) {
        unsafe {
            appine_mapper_destroy(self.0.as_ptr());
        }
    }
}

struct AndroidRenderer {
    service: Arc<Service>,
    mapper: Arc<Mapper>,
    caps: Vec<u8>,
}

pub(super) fn renderer(env: &mut JNIEnv) -> Option<Arc<dyn Renderer>> {
    let class = env.find_class("io/github/mthli/appine/GpuService").ok()?;
    let service = Arc::new(Service {
        vm: env.get_java_vm().ok()?,
        class: env.new_global_ref(class).ok()?,
    });
    // The probe runs in the private renderer process. The socket deadline also
    // bounds a driver stall during probe cleanup.
    let _probe = service.connect(true).ok()?;
    let mapper = Arc::new(Mapper(NonNull::new(unsafe { appine_mapper_create() })?));
    let mut caps = vec![0; unsafe { appine_gpu_capset(ptr::null_mut()) }];
    unsafe {
        appine_gpu_capset(caps.as_mut_ptr());
    }
    Some(Arc::new(AndroidRenderer {
        service,
        mapper,
        caps,
    }))
}

impl Renderer for AndroidRenderer {
    fn capset(&self) -> &[u8] {
        &self.caps
    }
    fn open(&self) -> Result<Arc<dyn Context>, Errno> {
        let channel = self.service.connect(false)?;
        let fd = channel.fd.try_clone().map_err(|_| Errno::EMFILE)?;
        let lock = Arc::new(Mutex::new(()));
        let worker_lock = lock.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("venus-retire".into())
            .spawn(move || retire(channel, &worker_lock, rx))
            .map_err(|_| Errno::EAGAIN)?;
        Ok(Arc::new(AndroidContext {
            fd,
            lock,
            mapper: self.mapper.clone(),
            tx,
        }))
    }
}

struct AndroidContext {
    fd: OwnedFd,
    lock: Arc<Mutex<()>>,
    mapper: Arc<Mapper>,
    tx: mpsc::Sender<Deferred>,
}
impl Drop for AndroidContext {
    fn drop(&mut self) {
        // Interrupt IPC immediately. The renderer drains outstanding GPU work before
        // destroying its Vulkan resources, and the retirement thread unbinds the service.
        unsafe {
            libc::shutdown(self.fd.as_raw_fd(), libc::SHUT_RDWR);
        }
        let _ = self.tx.send(Deferred::Disconnect);
    }
}

enum Deferred {
    Fence(OwnedFd, Vec<Arc<dyn Mapping>>),
    Release(Backing),
    Disconnect,
}
fn retire(channel: Channel, lock: &Mutex<()>, rx: mpsc::Receiver<Deferred>) {
    let mut channel = Some(channel);
    for job in rx {
        match job {
            Deferred::Fence(fd, buffers) => {
                let mut pfd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                while unsafe { libc::poll(&mut pfd, 1, -1) } < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                }
                drop(buffers);
            }
            Deferred::Release(backing) => {
                if let Some(channel) = &channel
                    && let Ok(_guard) = lock.lock()
                {
                    unsafe {
                        appine_gpu_release(channel.fd.as_raw_fd(), backing.id);
                    }
                }
                drop(backing);
            }
            Deferred::Disconnect => {
                channel.take();
            }
        }
    }
}

struct Backing {
    id: u32,
    mapper: Arc<Mapper>,
    memory: u64,
    base: NonNull<u8>,
    size: usize,
}
// The mapping is freed only by its last owner. No Rust reference aliases its bytes.
unsafe impl Send for Backing {}
unsafe impl Sync for Backing {}
impl Drop for Backing {
    fn drop(&mut self) {
        unsafe {
            appine_gpu_unmap(
                self.mapper.0.as_ptr(),
                self.memory,
                self.base.as_ptr(),
                self.size,
            );
        }
    }
}
struct AndroidMapping {
    backing: Option<Backing>,
    tx: mpsc::Sender<Deferred>,
}
unsafe impl Mapping for AndroidMapping {
    fn base(&self) -> NonNull<u8> {
        self.backing.as_ref().unwrap().base
    }
    fn size(&self) -> usize {
        self.backing.as_ref().unwrap().size
    }
}
impl Drop for AndroidMapping {
    fn drop(&mut self) {
        if let Some(backing) = self.backing.take() {
            let _ = self.tx.send(Deferred::Release(backing));
        }
    }
}

impl Context for AndroidContext {
    fn create(&self, resource: u32, blob: u64, size: u64) -> Result<Arc<dyn Mapping>, Errno> {
        let _guard = self.lock.lock().map_err(|_| Errno::EIO)?;
        let fd = self.fd.as_raw_fd();
        let (mut kind, mut index, mut allocation, mut handle) = (0, 0, 0, -1);
        let mut uuid = [0; 16];
        let result = unsafe {
            appine_gpu_create(
                fd,
                resource,
                blob,
                size,
                &mut kind,
                uuid.as_mut_ptr(),
                &mut index,
                &mut allocation,
                &mut handle,
            )
        };
        if result != 0 {
            return Err(Errno(result));
        }
        let handle = unsafe { OwnedFd::from_raw_fd(handle) };
        let mut memory = 0;
        let base = unsafe {
            appine_gpu_map(
                self.mapper.0.as_ptr(),
                handle.as_raw_fd(),
                blob,
                size,
                kind,
                uuid.as_ptr(),
                index,
                allocation,
                &mut memory,
            )
        };
        if let Some(base) = NonNull::new(base)
            && (base.as_ptr() as usize).is_multiple_of(4096)
        {
            return Ok(Arc::new(AndroidMapping {
                tx: self.tx.clone(),
                backing: Some(Backing {
                    id: resource,
                    mapper: self.mapper.clone(),
                    memory,
                    base,
                    size: size as usize,
                }),
            }));
        }
        if !base.is_null() {
            unsafe {
                appine_gpu_unmap(self.mapper.0.as_ptr(), memory, base, size as usize);
            }
        }
        unsafe {
            appine_gpu_release(fd, resource);
        }
        Err(Errno::EIO)
    }
    fn submit(
        &self,
        ring: u32,
        command: &[u8],
        fence: bool,
        buffers: Vec<Arc<dyn Mapping>>,
    ) -> Result<Option<OwnedFd>, Errno> {
        let _guard = self.lock.lock().map_err(|_| Errno::EIO)?;
        let needs_fence = fence || !buffers.is_empty();
        let mut out = -1;
        let result = unsafe {
            appine_gpu_submit(
                self.fd.as_raw_fd(),
                ring,
                command.as_ptr(),
                command.len(),
                if needs_fence {
                    &mut out
                } else {
                    ptr::null_mut()
                },
            )
        };
        if result != 0 {
            return Err(Errno(result));
        }
        if !needs_fence {
            return Ok(None);
        }
        let fd = unsafe { OwnedFd::from_raw_fd(out) };
        if buffers.is_empty() {
            return Ok(Some(fd));
        }
        let guest = fence
            .then(|| fd.try_clone().map_err(|_| Errno::EMFILE))
            .transpose();
        self.tx
            .send(Deferred::Fence(fd, buffers))
            .map_err(|_| Errno::EIO)?;
        guest
    }
}
