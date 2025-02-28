// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <inttypes.h>
#include <lib/ddk/hw/inout.h>
#include <lib/pci/pio.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <limits.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <threads.h>
#include <zircon/assert.h>
#include <zircon/process.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/time.h>

#include <ctime>
#include <memory>
#include <new>
#include <utility>

#include <bitmap/raw-bitmap.h>
#include <bitmap/storage.h>
#include <fbl/alloc_checker.h>
#include <fbl/auto_lock.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/intrusive_hash_table.h>
#include <fbl/intrusive_single_list.h>

#if !defined(__x86_64__) && !defined(__x86__)
#error "Unsupported architecture"
#endif

#include <acpica/acpi.h>

__WEAK zx_handle_t root_resource_handle;

#define _COMPONENT ACPI_OS_SERVICES
ACPI_MODULE_NAME("oszircon")

#define UNIMPLEMENTED() ZX_PANIC("%s unimplemented\n", __func__)

#define LOCAL_TRACE 0

#define TRACEF(str, x...)                               \
  do {                                                  \
    printf("%s:%d: " str, __FUNCTION__, __LINE__, ##x); \
  } while (0)
#define LTRACEF(x...)  \
  do {                 \
    if (LOCAL_TRACE) { \
      TRACEF(x);       \
    }                  \
  } while (0)

/* Structures used for implementing AcpiOsExecute and
 * AcpiOsWaitEventsComplete */
struct AcpiOsTaskCtx : public fbl::DoublyLinkedListable<std::unique_ptr<AcpiOsTaskCtx>> {
  ACPI_OSD_EXEC_CALLBACK func;
  void* ctx;
};

/* Thread function for implementing AcpiOsExecute */
static int AcpiOsExecuteTask(void* arg);
/* Tear down the OsExecuteTask thread */
static void ShutdownOsExecuteTask();

/* Data used for implementing AcpiOsExecute and
 * AcpiOsWaitEventsComplete */
static struct {
  thrd_t thread;
  cnd_t cond;
  cnd_t idle_cond;
  mtx_t lock = MTX_INIT;
  bool shutdown = false;
  bool idle = true;

  fbl::DoublyLinkedList<std::unique_ptr<AcpiOsTaskCtx>> tasks;
} os_execute_state;

class AcpiOsMappingNode : public fbl::SinglyLinkedListable<std::unique_ptr<AcpiOsMappingNode>> {
 public:
  using HashTable = fbl::HashTable<uintptr_t, std::unique_ptr<AcpiOsMappingNode>>;

  // @param vaddr Virtual address returned to ACPI, used as key to the hashtable.
  // @param vaddr_actual Actual virtual address of the mapping. May be different than
  //                     vaddr if it is unaligned.
  // @param length Length of the mapping
  // @param vmo_handle Handle to the mapped VMO
  AcpiOsMappingNode(uintptr_t vaddr, uintptr_t vaddr_actual, size_t length, zx_handle_t vmo_handle);
  ~AcpiOsMappingNode();

  // Trait implementation for fbl::HashTable
  uintptr_t GetKey() const { return vaddr_; }
  static size_t GetHash(uintptr_t key) { return key; }

 private:
  uintptr_t vaddr_;
  uintptr_t vaddr_actual_;
  size_t length_;
  zx_handle_t vmo_handle_;
};

fbl::Mutex os_mapping_lock;

AcpiOsMappingNode::HashTable os_mapping_tbl;

const size_t PCIE_MAX_DEVICES_PER_BUS = 32;
const size_t PCIE_MAX_FUNCTIONS_PER_DEVICE = 8;

AcpiOsMappingNode::AcpiOsMappingNode(uintptr_t vaddr, uintptr_t vaddr_actual, size_t length,
                                     zx_handle_t vmo_handle)
    : vaddr_(vaddr), vaddr_actual_(vaddr_actual), length_(length), vmo_handle_(vmo_handle) {}

AcpiOsMappingNode::~AcpiOsMappingNode() {
  zx_vmar_unmap(zx_vmar_root_self(), (uintptr_t)vaddr_actual_, length_);
  zx_handle_close(vmo_handle_);
}

static zx_status_t mmap_physical(zx_paddr_t phys, size_t size, uint32_t cache_policy,
                                 zx_handle_t* out_vmo, zx_vaddr_t* out_vaddr) {
  zx_handle_t vmo;
  zx_vaddr_t vaddr;
  zx_status_t st = zx_vmo_create_physical(root_resource_handle, phys, size, &vmo);
  if (st != ZX_OK) {
    return st;
  }
  st = zx_vmo_set_cache_policy(vmo, cache_policy);
  if (st != ZX_OK) {
    zx_handle_close(vmo);
    return st;
  }
  st = zx_vmar_map(zx_vmar_root_self(), ZX_VM_PERM_READ | ZX_VM_PERM_WRITE | ZX_VM_MAP_RANGE, 0,
                   vmo, 0, size, &vaddr);
  if (st != ZX_OK) {
    zx_handle_close(vmo);
    return st;
  } else {
    *out_vmo = vmo;
    *out_vaddr = vaddr;
    return ZX_OK;
  }
}

static ACPI_STATUS thrd_status_to_acpi_status(int status) {
  switch (status) {
    case thrd_success:
      return AE_OK;
    case thrd_nomem:
      return AE_NO_MEMORY;
    case thrd_timedout:
      return AE_TIME;
    default:
      return AE_ERROR;
  }
}

static std::timespec timeout_to_timespec(UINT16 Timeout) {
  std::timespec ts;
  ZX_ASSERT(std::timespec_get(&ts, TIME_UTC) != 0);
  return zx_timespec_from_duration(
      zx_duration_add_duration(zx_duration_from_timespec(ts), ZX_MSEC(Timeout)));
}

// The |acpi_spinlock_lock| is used to guarantee that all spinlock acquisitions will
// be uncontested in certain circumstances.  This allows us to ensure that
// the codepaths for entering an S-state will not need to wait for some other thread
// to finish processing.  The scheme works with the following protocol:
//
// Normal operational threads: If attempting to acquire a lock, and the thread
// holds no spinlock yet, then acquire |acpi_spinlock_lock| in READ mode before
// acquiring the desired lock.  For all other lock acquisitions behave normally.
// If a thread is releasing its last held lock, release the |acpi_spinlock_lock|.
//
// Non-contested thread: To enter non-contested mode, call
// |acpica_enable_noncontested_mode| while not holding any ACPI spinlock.  This will
// acquire the |acpi_spinlock_lock| in WRITE mode.  Call
// |acpica_disable_noncontested_mode| while not holding any ACPI spinlock to release
// the |acpi_spinlock_lock|.
//
// Non-contested mode needs to apply to both spin locks and mutexes to prevent deadlock.
static pthread_rwlock_t acpi_spinlock_lock = PTHREAD_RWLOCK_INITIALIZER;
static thread_local uint64_t acpi_spinlocks_held = 0;

void acpica_enable_noncontested_mode() {
  ZX_ASSERT(acpi_spinlocks_held == 0);
  int ret = pthread_rwlock_wrlock(&acpi_spinlock_lock);
  ZX_ASSERT(ret == 0);
  acpi_spinlocks_held++;
}

void acpica_disable_noncontested_mode() {
  ZX_ASSERT(acpi_spinlocks_held == 1);
  int ret = pthread_rwlock_unlock(&acpi_spinlock_lock);
  ZX_ASSERT(ret == 0);
  acpi_spinlocks_held--;
}

static void initialize_port_bitmap();
static zx_status_t handle_port_permissions(uint16_t address, UINT32 width_bits);
static ACPI_STATUS zx_status_to_acpi_status(zx_status_t st);

/**
 * @brief Initialize the OSL subsystem.
 *
 * This function allows the OSL to initialize itself.  It is called during
 * intiialization of the ACPICA subsystem.
 *
 * @return Initialization status
 */
ACPI_STATUS AcpiOsInitialize() {
  ACPI_STATUS status = thrd_status_to_acpi_status(cnd_init(&os_execute_state.cond));
  if (status != AE_OK) {
    return status;
  }
  status = thrd_status_to_acpi_status(cnd_init(&os_execute_state.idle_cond));
  if (status != AE_OK) {
    cnd_destroy(&os_execute_state.cond);
    return status;
  }

  status =
      thrd_status_to_acpi_status(thrd_create(&os_execute_state.thread, AcpiOsExecuteTask, nullptr));
  if (status != AE_OK) {
    return status;
  }

  initialize_port_bitmap();

  // For AcpiOsWritePort and AcpiOsReadPort to operate they need access to ioports 0xCF8 and 0xCFC
  // per the Pci Local Bus specification v3.0. Each address is a 32 bit port.
  for (const auto addr : {kPciConfigAddrPort, kPciConfigDataPort}) {
    zx_status_t pio_status = handle_port_permissions(addr, 32);
    if (pio_status != ZX_OK) {
      return zx_status_to_acpi_status(pio_status);
    }
  }

  return AE_OK;
}

/**
 * @brief Terminate the OSL subsystem.
 *
 * This function allows the OSL to cleanup and terminate.  It is called during
 * termination of the ACPICA subsystem.
 *
 * @return Termination status
 */
ACPI_STATUS AcpiOsTerminate() {
  ShutdownOsExecuteTask();
  cnd_destroy(&os_execute_state.cond);
  cnd_destroy(&os_execute_state.idle_cond);

  return AE_OK;
}

/**
 * @brief Obtain the Root ACPI table pointer (RSDP).
 *
 * @return The physical address of the RSDP
 */
ACPI_PHYSICAL_ADDRESS AcpiOsGetRootPointer() {
  zx_paddr_t acpi_rsdp, smbios;
  zx_status_t zx_status = zx_pc_firmware_tables(root_resource_handle, &acpi_rsdp, &smbios);
  if (zx_status == ZX_OK && acpi_rsdp != 0) {
    return acpi_rsdp;
  }

  ACPI_PHYSICAL_ADDRESS TableAddress = 0;
  ACPI_STATUS status = AcpiFindRootPointer(&TableAddress);
  if (status != AE_OK) {
    return 0;
  }
  return TableAddress;
}

/**
 * @brief Allow the host OS to override a predefined ACPI object.
 *
 * @param PredefinedObject A pointer to a predefind object (name and initial
 *        value)
 * @param NewValue Where a new value for the predefined object is returned.
 *        NULL if there is no override for this object.
 *
 * @return Exception code that indicates success or reason for failure.
 */
ACPI_STATUS AcpiOsPredefinedOverride(const ACPI_PREDEFINED_NAMES* PredefinedObject,
                                     ACPI_STRING* NewValue) {
  *NewValue = NULL;
  return AE_OK;
}

/**
 * @brief Allow the host OS to override a firmware ACPI table via a logical
 *        address.
 *
 * @param ExistingTable A pointer to the header of the existing ACPI table
 * @param NewTable Where the pointer to the replacment table is returned.  The
 *        OSL returns NULL if no replacement is provided.
 *
 * @return Exception code that indicates success or reason for failure.
 */
ACPI_STATUS AcpiOsTableOverride(ACPI_TABLE_HEADER* ExistingTable, ACPI_TABLE_HEADER** NewTable) {
  *NewTable = NULL;
  return AE_OK;
}

/**
 * @brief Allow the host OS to override a firmware ACPI table via a physical
 *        address.
 *
 * @param ExistingTable A pointer to the header of the existing ACPI table
 * @param NewAddress Where the physical address of the replacment table is
 *        returned.  The OSL returns NULL if no replacement is provided.
 * @param NewLength Where the length of the replacement table is returned.
 *
 * @return Exception code that indicates success or reason for failure.
 */
ACPI_STATUS AcpiOsPhysicalTableOverride(ACPI_TABLE_HEADER* ExistingTable,
                                        ACPI_PHYSICAL_ADDRESS* NewAddress, UINT32* NewTableLength) {
  *NewAddress = 0;
  return AE_OK;
}

// If we decide to make use of a more Zircon specific cache mechanism,
// remove the ACPI_USE_LOCAL_CACHE define from the header and implement these
// functions.
#if 0
/**
 * @brief Create a memory cache object.
 *
 * @param CacheName An ASCII identfier for the cache.
 * @param ObjectSize The size of each object in the cache.
 * @param MaxDepth Maximum number of objects in the cache.
 * @param ReturnCache Where a pointer to the cache object is returned.
 *
 * @return AE_OK The cache was successfully created.
 * @return AE_BAD_PARAMETER The ReturnCache pointer is NULL or ObjectSize < 16.
 * @return AE_NO_MEMORY Insufficient dynamic memory to complete the operation.
 */
ACPI_STATUS AcpiOsCreateCache(
        char *CacheName,
        UINT16 ObjectSize,
        UINT16 MaxDepth,
        ACPI_CACHE_T **ReturnCache) {
    PANIC_UNIMPLEMENTED;
    return AE_NO_MEMORY;
}

/**
 * @brief Delete a memory cache object.
 *
 * @param Cache The cache object to be deleted.
 *
 * @return AE_OK The cache was successfully deleted.
 * @return AE_BAD_PARAMETER The Cache pointer is NULL.
 */
ACPI_STATUS AcpiOsDeleteCache(ACPI_CACHE_T *Cache) {
    PANIC_UNIMPLEMENTED;
    return AE_OK;
}

/**
 * @brief Free all objects currently within a cache object.
 *
 * @param Cache The cache object to purge.
 *
 * @return AE_OK The cache was successfully purged.
 * @return AE_BAD_PARAMETER The Cache pointer is NULL.
 */
ACPI_STATUS AcpiOsPurgeCache(ACPI_CACHE_T *Cache) {
    PANIC_UNIMPLEMENTED;
    return AE_OK;
}


/**
 * @brief Acquire an object from a cache.
 *
 * @param Cache The cache object from which to acquire an object.
 *
 * @return A pointer to a cache object. NULL if the object could not be
 *         acquired.
 */
void *AcpiOsAcquireObject(ACPI_CACHE_T *Cache) {
    PANIC_UNIMPLEMENTED;
    return NULL;
}

/**
 * @brief Release an object to a cache.
 *
 * @param Cache The cache object to which the object will be released.
 * @param Object The object to be released.
 *
 * @return AE_OK The object was successfully released.
 * @return AE_BAD_PARAMETER The Cache or Object pointer is NULL.
 */
ACPI_STATUS AcpiOsReleaseObject(ACPI_CACHE_T *Cache, void *Object) {
    PANIC_UNIMPLEMENTED;
    return AE_OK;
}
#endif

/**
 * @brief Map physical memory into the caller's address space.
 *
 * @param PhysicalAddress A full physical address of the memory to be mapped
 *        into the caller's address space
 * @param Length The amount of memory to mapped starting at the given physical
 *        address
 *
 * @return Logical pointer to the mapped memory. A NULL pointer indicated failures.
 */
void* AcpiOsMapMemory(ACPI_PHYSICAL_ADDRESS PhysicalAddress, ACPI_SIZE Length) {
  fbl::AutoLock lock(&os_mapping_lock);

  // Caution: PhysicalAddress might not be page-aligned, Length might not
  // be a page multiple.

  const size_t kPageSize = zx_system_get_page_size();
  ACPI_PHYSICAL_ADDRESS aligned_address = PhysicalAddress & ~(kPageSize - 1);
  ACPI_PHYSICAL_ADDRESS end = (PhysicalAddress + Length + kPageSize - 1) & ~(kPageSize - 1);

  uintptr_t vaddr;
  size_t length = end - aligned_address;
  zx_handle_t vmo;
  zx_status_t status =
      mmap_physical(aligned_address, end - aligned_address, ZX_CACHE_POLICY_CACHED, &vmo, &vaddr);
  if (status != ZX_OK) {
    return NULL;
  }

  void* out_addr = (void*)(vaddr + (PhysicalAddress - aligned_address));
  std::unique_ptr<AcpiOsMappingNode> mn(
      new AcpiOsMappingNode(reinterpret_cast<uintptr_t>(out_addr), vaddr, length, vmo));
  os_mapping_tbl.insert(std::move(mn));

  return out_addr;
}

/**
 * @brief Remove a physical to logical memory mapping.
 *
 * @param LogicalAddress The logical address that was returned from a previous
 *        call to AcpiOsMapMemory.
 * @param Length The amount of memory that was mapped. This value must be
 *        identical to the value used in the call to AcpiOsMapMemory.
 */
void AcpiOsUnmapMemory(void* LogicalAddress, ACPI_SIZE Length) {
  fbl::AutoLock lock(&os_mapping_lock);
  std::unique_ptr<AcpiOsMappingNode> mn = os_mapping_tbl.erase((uintptr_t)LogicalAddress);
  if (mn == NULL) {
    printf("AcpiOsUnmapMemory nonexisting mapping %p\n", LogicalAddress);
  }
}

/**
 * @brief Allocate memory from the dynamic memory pool.
 *
 * @param Size Amount of memory to allocate.
 *
 * @return A pointer to the allocated memory. A NULL pointer is returned on
 *         error.
 */
void* AcpiOsAllocate(ACPI_SIZE Size) { return malloc(Size); }

/**
 * @brief Free previously allocated memory.
 *
 * @param Memory A pointer to the memory to be freed.
 */
void AcpiOsFree(void* Memory) { free(Memory); }

/**
 * @brief Obtain the ID of the currently executing thread.
 *
 * @return A unique non-zero value that represents the ID of the currently
 *         executing thread. The value -1 is reserved and must not be returned
 *         by this interface.
 */
static_assert(sizeof(ACPI_THREAD_ID) >= sizeof(zx_handle_t), "tid size");
ACPI_THREAD_ID AcpiOsGetThreadId() { return (uintptr_t)thrd_current(); }

static int AcpiOsExecuteTask(void* arg) {
  while (1) {
    std::unique_ptr<AcpiOsTaskCtx> task;

    mtx_lock(&os_execute_state.lock);
    while ((task = os_execute_state.tasks.pop_front()) == nullptr) {
      os_execute_state.idle = true;
      // If anything is waiting for the queue to empty, notify it.
      cnd_signal(&os_execute_state.idle_cond);

      // If we're waiting to shutdown, do it now that there's no more work
      if (os_execute_state.shutdown) {
        mtx_unlock(&os_execute_state.lock);
        return 0;
      }

      cnd_wait(&os_execute_state.cond, &os_execute_state.lock);
    }
    os_execute_state.idle = false;
    mtx_unlock(&os_execute_state.lock);

    task->func(task->ctx);
  }

  return 0;
}

static void ShutdownOsExecuteTask() {
  mtx_lock(&os_execute_state.lock);
  os_execute_state.shutdown = true;
  mtx_unlock(&os_execute_state.lock);
  cnd_broadcast(&os_execute_state.cond);
  thrd_join(os_execute_state.thread, nullptr);
}

/**
 * @brief Schedule a procedure for deferred execution.
 *
 * @param Type Type of the callback function.
 * @param Function Address of the procedure to execute.
 * @param Context A context value to be passed to the called procedure.
 *
 * @return AE_OK The procedure was successfully queued for execution.
 * @return AE_BAD_PARAMETER The Type is invalid or the Function pointer
 *         is NULL.
 */
ACPI_STATUS AcpiOsExecute(ACPI_EXECUTE_TYPE Type, ACPI_OSD_EXEC_CALLBACK Function, void* Context) {
  if (Function == NULL) {
    return AE_BAD_PARAMETER;
  }

  switch (Type) {
    case OSL_GLOBAL_LOCK_HANDLER:
    case OSL_NOTIFY_HANDLER:
    case OSL_GPE_HANDLER:
    case OSL_DEBUGGER_MAIN_THREAD:
    case OSL_DEBUGGER_EXEC_THREAD:
    case OSL_EC_POLL_HANDLER:
    case OSL_EC_BURST_HANDLER:
      break;
    default:
      return AE_BAD_PARAMETER;
  }

  fbl::AllocChecker ac;
  std::unique_ptr<AcpiOsTaskCtx> task(new (&ac) AcpiOsTaskCtx);
  if (!ac.check()) {
    return AE_NO_MEMORY;
  }
  task->func = Function;
  task->ctx = Context;

  mtx_lock(&os_execute_state.lock);
  os_execute_state.tasks.push_back(std::move(task));
  mtx_unlock(&os_execute_state.lock);
  cnd_signal(&os_execute_state.cond);

  return AE_OK;
}

/**
 * @brief Wait for completion of asynchronous events.
 *
 * This function blocks until all asynchronous events initiated by
 * AcpiOsExecute have completed.
 */
void AcpiOsWaitEventsComplete(void) {
  mtx_lock(&os_execute_state.lock);
  while (!os_execute_state.idle) {
    cnd_wait(&os_execute_state.idle_cond, &os_execute_state.lock);
  }
  mtx_unlock(&os_execute_state.lock);
}

/**
 * @brief Suspend the running task (course granularity).
 *
 * @param Milliseconds The amount of time to sleep, in milliseconds.
 */
void AcpiOsSleep(UINT64 Milliseconds) {
  if (Milliseconds > UINT32_MAX) {
    // If we're asked to sleep for a long time (>1.5 months), shorten it
    Milliseconds = UINT32_MAX;
  }
  zx_nanosleep(zx_deadline_after(ZX_MSEC(Milliseconds)));
}

/**
 * @brief Wait for a short amount of time (fine granularity).
 *
 * Execution of the running thread is not suspended for this time.
 *
 * @param Microseconds The amount of time to delay, in microseconds.
 */
void AcpiOsStall(UINT32 Microseconds) { zx_nanosleep(zx_deadline_after(ZX_USEC(Microseconds))); }

/**
 * @brief Create a semaphore.
 *
 * @param MaxUnits The maximum number of units this semaphore will be required
 *        to accept
 * @param InitialUnits The initial number of units to be assigned to the
 *        semaphore.
 * @param OutHandle A pointer to a locaton where a handle to the semaphore is
 *        to be returned.
 *
 * @return AE_OK The semaphore was successfully created.
 * @return AE_BAD_PARAMETER The InitialUnits is invalid or the OutHandle
 *         pointer is NULL.
 * @return AE_NO_MEMORY Insufficient memory to create the semaphore.
 */
ACPI_STATUS AcpiOsCreateSemaphore(UINT32 MaxUnits, UINT32 InitialUnits, ACPI_SEMAPHORE* OutHandle) {
  sem_t* sem = (sem_t*)malloc(sizeof(sem_t));
  if (!sem) {
    return AE_NO_MEMORY;
  }
  if (sem_init(sem, 0, InitialUnits) < 0) {
    free(sem);
    return AE_ERROR;
  }
  *OutHandle = sem;
  return AE_OK;
}

/**
 * @brief Delete a semaphore.
 *
 * @param Handle A handle to a semaphore objected that was returned by a
 *        previous call to AcpiOsCreateSemaphore.
 *
 * @return AE_OK The semaphore was successfully deleted.
 */
ACPI_STATUS AcpiOsDeleteSemaphore(ACPI_SEMAPHORE Handle) {
  free(Handle);
  return AE_OK;
}

/**
 * @brief Wait for units from a semaphore.
 *
 * @param Handle A handle to a semaphore objected that was returned by a
 *        previous call to AcpiOsCreateSemaphore.
 * @param Units The number of units the caller is requesting.
 * @param Timeout How long the caller is willing to wait for the requested
 *        units, in milliseconds.  A value of -1 indicates that the caller
 *        is willing to wait forever. Timeout may be 0.
 *
 * @return AE_OK The requested units were successfully received.
 * @return AE_BAD_PARAMETER The Handle is invalid.
 * @return AE_TIME The units could not be acquired within the specified time.
 */
ACPI_STATUS AcpiOsWaitSemaphore(ACPI_SEMAPHORE Handle, UINT32 Units, UINT16 Timeout) {
  if (Timeout == UINT16_MAX) {
    if (sem_wait(Handle) < 0) {
      ZX_ASSERT_MSG(false, "sem_wait failed %d", errno);
    }
    return AE_OK;
  }

  std::timespec then = timeout_to_timespec(Timeout);
  if (sem_timedwait(Handle, &then) < 0) {
    ZX_ASSERT_MSG(errno == ETIMEDOUT, "sem_timedwait failed unexpectedly %d", errno);
    return AE_TIME;
  }
  return AE_OK;
}

/**
 * @brief Send units to a semaphore.
 *
 * @param Handle A handle to a semaphore objected that was returned by a
 *        previous call to AcpiOsCreateSemaphore.
 * @param Units The number of units to send to the semaphore.
 *
 * @return AE_OK The semaphore was successfully signaled.
 * @return AE_BAD_PARAMETER The Handle is invalid.
 */
ACPI_STATUS AcpiOsSignalSemaphore(ACPI_SEMAPHORE Handle, UINT32 Units) {
  // TODO: Implement support for Units > 1
  ZX_DEBUG_ASSERT(Units == 1);

  sem_post(Handle);
  return AE_OK;
}

/**
 * @brief Create a mutex.
 *
 * @param OutHandle A pointer to a locaton where a handle to the mutex is
 *        to be returned.
 *
 * @return AE_OK The mutex was successfully created.
 * @return AE_BAD_PARAMETER The OutHandle pointer is NULL.
 * @return AE_NO_MEMORY Insufficient memory to create the mutex.
 */
ACPI_STATUS AcpiOsCreateMutex(ACPI_MUTEX* OutHandle) {
  mtx_t* lock = (mtx_t*)malloc(sizeof(mtx_t));
  if (!lock) {
    return AE_NO_MEMORY;
  }

  ACPI_STATUS status = thrd_status_to_acpi_status(mtx_init(lock, mtx_plain));
  if (status != AE_OK) {
    return status;
  }
  *OutHandle = lock;
  return AE_OK;
}

/**
 * @brief Delete a mutex.
 *
 * @param Handle A handle to a mutex objected that was returned by a
 *        previous call to AcpiOsCreateMutex.
 */
void AcpiOsDeleteMutex(ACPI_MUTEX Handle) {
  mtx_destroy(Handle);
  free(Handle);
}

/**
 * @brief Acquire a mutex.
 *
 * @param Handle A handle to a mutex objected that was returned by a
 *        previous call to AcpiOsCreateMutex.
 * @param Timeout How long the caller is willing to wait for the requested
 *        units, in milliseconds.  A value of -1 indicates that the caller
 *        is willing to wait forever. Timeout may be 0.
 *
 * @return AE_OK The requested units were successfully received.
 * @return AE_BAD_PARAMETER The Handle is invalid.
 * @return AE_TIME The mutex could not be acquired within the specified time.
 */
ACPI_STATUS AcpiOsAcquireMutex(ACPI_MUTEX Handle, UINT16 Timeout)
    TA_TRY_ACQ(AE_OK, Handle) TA_NO_THREAD_SAFETY_ANALYSIS {
  if (Timeout == UINT16_MAX) {
    if (acpi_spinlocks_held == 0) {
      int ret = pthread_rwlock_rdlock(&acpi_spinlock_lock);
      ZX_ASSERT(ret == 0);
    }

    int res = mtx_lock(Handle);
    ZX_ASSERT(res == thrd_success);
  } else {
    std::timespec then = timeout_to_timespec(Timeout);

    if (acpi_spinlocks_held == 0) {
      int ret = pthread_rwlock_timedrdlock(&acpi_spinlock_lock, &then);
      if (ret == ETIMEDOUT)
        return AE_TIME;
      ZX_ASSERT(ret == 0);
    }

    int res = mtx_timedlock(Handle, &then);
    if (res == thrd_timedout) {
      if (acpi_spinlocks_held == 0) {
        int res = pthread_rwlock_unlock(&acpi_spinlock_lock);
        ZX_ASSERT(res == 0);
      }
      return AE_TIME;
    }
    ZX_ASSERT(res == thrd_success);
  }

  acpi_spinlocks_held++;
  return AE_OK;
}

/**
 * @brief Release a mutex.
 *
 * @param Handle A handle to a mutex objected that was returned by a
 *        previous call to AcpiOsCreateMutex.
 */
void AcpiOsReleaseMutex(ACPI_MUTEX Handle) TA_REL(Handle) {
  mtx_unlock(Handle);

  acpi_spinlocks_held--;
  if (acpi_spinlocks_held == 0) {
    int ret = pthread_rwlock_unlock(&acpi_spinlock_lock);
    ZX_ASSERT(ret == 0);
  }
}

/**
 * @brief Create a spin lock.
 *
 * @param OutHandle A pointer to a locaton where a handle to the lock is
 *        to be returned.
 *
 * @return AE_OK The lock was successfully created.
 * @return AE_BAD_PARAMETER The OutHandle pointer is NULL.
 * @return AE_NO_MEMORY Insufficient memory to create the lock.
 */
ACPI_STATUS AcpiOsCreateLock(ACPI_SPINLOCK* OutHandle) {
  // Since we don't have a notion of interrupt contex in usermode, just make
  // these mutexes.
  return AcpiOsCreateMutex(OutHandle);
}

/**
 * @brief Delete a spin lock.
 *
 * @param Handle A handle to a lock objected that was returned by a
 *        previous call to AcpiOsCreateLock.
 */
void AcpiOsDeleteLock(ACPI_SPINLOCK Handle) { AcpiOsDeleteMutex(Handle); }

/**
 * @brief Acquire a spin lock.
 *
 * @param Handle A handle to a lock objected that was returned by a
 *        previous call to AcpiOsCreateLock.
 *
 * @return Platform-dependent CPU flags.  To be used when the lock is released.
 */
ACPI_CPU_FLAGS AcpiOsAcquireLock(ACPI_SPINLOCK Handle) TA_ACQ(Handle) TA_NO_THREAD_SAFETY_ANALYSIS {
  int ret = AcpiOsAcquireMutex(Handle, UINT16_MAX);
  // The thread safety analysis doesn't seem to handle the noreturn inside of the assert
  ZX_ASSERT(ret == AE_OK);
  return 0;
}

/**
 * @brief Release a spin lock.
 *
 * @param Handle A handle to a lock objected that was returned by a
 *        previous call to AcpiOsCreateLock.
 * @param Flags CPU Flags that were returned from AcpiOsAcquireLock.
 */
void AcpiOsReleaseLock(ACPI_SPINLOCK Handle, ACPI_CPU_FLAGS Flags) TA_REL(Handle) {
  AcpiOsReleaseMutex(Handle);
}

// Wrapper structs for interfacing between our interrupt handler convention and
// ACPICA's
struct AcpiIrqThread {
  thrd_t thread;
  ACPI_OSD_HANDLER handler;
  zx_handle_t irq_handle;
  void* context;
};
static int acpi_irq_thread(void* arg) {
  auto real_arg = static_cast<AcpiIrqThread*>(arg);
  while (1) {
    zx_status_t status = zx_interrupt_wait(real_arg->irq_handle, NULL);
    if (status != ZX_OK) {
      break;
    }
    // TODO: Should we do something with the return value from the handler?
    real_arg->handler(real_arg->context);
  }
  return 0;
}

static std::unique_ptr<AcpiIrqThread> sci_irq;

/**
 * @brief Install a handler for a hardware interrupt.
 *
 * @param InterruptLevel Interrupt level that the handler will service.
 * @param Handler Address of the handler.
 * @param Context A context value that is passed to the handler when the
 *        interrupt is dispatched.
 *
 * @return AE_OK The handler was successfully installed.
 * @return AE_BAD_PARAMETER The InterruptNumber is invalid or the Handler
 *         pointer is NULL.
 * @return AE_ALREADY_EXISTS A handler for this interrupt level is already
 *         installed.
 */
ACPI_STATUS AcpiOsInstallInterruptHandler(UINT32 InterruptLevel, ACPI_OSD_HANDLER Handler,
                                          void* Context) {
  // Note that InterruptLevel here is ISA IRQs (or global of the legacy PIC
  // does't exist), not system exceptions.

  // TODO: Clean this up to be less x86 centric.

  if (InterruptLevel == 0) {
    /* Some buggy firmware fails to populate the SCI_INT field of the FADT
     * properly.  0 is a known bad value, since the legacy PIT uses it and
     * cannot be remapped.  Just lie and say we installed a handler; this
     * system will just never receive an SCI.  If we return an error here,
     * ACPI init will fail completely, and the system will be unusable. */
    return AE_OK;
  }

  ZX_DEBUG_ASSERT(InterruptLevel == 0x9);  // SCI

  fbl::AllocChecker ac;
  std::unique_ptr<AcpiIrqThread> arg(new (&ac) AcpiIrqThread());
  if (!ac.check()) {
    return AE_NO_MEMORY;
  }

  zx_handle_t handle;
  zx_status_t status =
      zx_interrupt_create(root_resource_handle, InterruptLevel, ZX_INTERRUPT_REMAP_IRQ, &handle);
  if (status != ZX_OK) {
    return AE_ERROR;
  }
  arg->handler = Handler;
  arg->context = Context;
  arg->irq_handle = handle;

  int ret = thrd_create(&arg->thread, acpi_irq_thread, arg.get());
  if (ret != 0) {
    return AE_ERROR;
  }

  sci_irq = std::move(arg);
  return AE_OK;
}

/**
 * @brief Remove an interrupt handler.
 *
 * @param InterruptNumber Interrupt number that the handler is currently
 *        servicing.
 * @param Handler Address of the handler that was previously installed.
 *
 * @return AE_OK The handler was successfully removed.
 * @return AE_BAD_PARAMETER The InterruptNumber is invalid, the Handler
 *         pointer is NULL, or the Handler address is no the same as the one
 *         currently installed.
 * @return AE_NOT_EXIST There is no handler installed for this interrupt level.
 */
ACPI_STATUS AcpiOsRemoveInterruptHandler(UINT32 InterruptNumber, ACPI_OSD_HANDLER Handler) {
  ZX_DEBUG_ASSERT(InterruptNumber == 0x9);  // SCI
  ZX_DEBUG_ASSERT(sci_irq);
  zx_interrupt_destroy(sci_irq->irq_handle);
  thrd_join(sci_irq->thread, nullptr);
  sci_irq.reset();
  return AE_OK;
}

/**
 * @brief Read a value from a memory location.
 *
 * @param Address Memory address to be read.
 * @param Value A pointer to a location where the data is to be returned.
 * @param Width The memory width in bits, either 8, 16, 32, or 64.
 *
 * @return Exception code that indicates success or reason for failure.
 */
ACPI_STATUS AcpiOsReadMemory(ACPI_PHYSICAL_ADDRESS Address, UINT64* Value, UINT32 Width) {
  UNIMPLEMENTED();
  return AE_OK;
}

/**
 * @brief Write a value to a memory location.
 *
 * @param Address Memory address where data is to be written.
 * @param Value Data to be written to the memory location.
 * @param Width The memory width in bits, either 8, 16, 32, or 64.
 *
 * @return Exception code that indicates success or reason for failure.
 */
ACPI_STATUS AcpiOsWriteMemory(ACPI_PHYSICAL_ADDRESS Address, UINT64 Value, UINT32 Width) {
  UNIMPLEMENTED();
  return AE_OK;
}

// Essentially, we're using a bitmap here to represent each individual I/O port, so that we can
// keep track of which I/O ports are allowed and which are not by the kernel.

static constexpr size_t max_io_port = UINT16_MAX;
static constexpr size_t io_port_bitmap_size = max_io_port + 1;
static fbl::Mutex bitmap_lock;
static bitmap::RawBitmapGeneric<bitmap::FixedStorage<io_port_bitmap_size>> port_bitmap;

static void initialize_port_bitmap() {
  // This cannot fail given that we're using fixed storage
  port_bitmap.Reset(io_port_bitmap_size);
}

static bool check_port_permissions(uint16_t address, uint8_t width_bytes) {
  LTRACEF("Testing %#x until %#x, in bitmap of size %#zx\n", address, address + width_bytes,
          port_bitmap.size());

  return port_bitmap.Scan(address, address + width_bytes, true);
}

/**
 * @brief Make the I/O ports accessible and set them in the bitmap, so that we don't call
 * the kernel again.
 *
 * @param address The I/O address.
 * @param width_bytes The width of the access, in bytes.
 *
 * @return Status code that indicates success or reason for error.
 */
static zx_status_t add_port_permissions(uint16_t address, uint8_t width_bytes) {
  zx_status_t result = port_bitmap.Set(address, address + width_bytes);
  ZX_ASSERT(result == ZX_OK);

  LTRACEF("Adding permissions to [%#x, %#x]\n", address, address + width_bytes);

  return zx_ioports_request(root_resource_handle, address, width_bytes);
}

/**
 * @brief Handle all matters of I/O port permissions with the kernel.
 *
 * @param address The I/O address.
 * @param width_bits The width of the access, in bits.
 *
 * @return Status code that indicates success or reason for error.
 */
static zx_status_t handle_port_permissions(uint16_t address, UINT32 width_bits) {
  // It's a good idea to convert bits to bytes here, considering each
  // I/O port "byte" has its own bit in the bitmap
  uint8_t width_bytes = static_cast<uint8_t>(width_bits / 8);

  fbl::AutoLock g{&bitmap_lock};

  if (!check_port_permissions(address, width_bytes)) {
    // If the port is disallowed at the moment, call the kernel so it isn't
    return add_port_permissions(address, width_bytes);
  } else {
    LTRACEF("port %#x(width %#x) was already set.\n", address, width_bytes);
  }

  return ZX_OK;
}

static ACPI_STATUS zx_status_to_acpi_status(zx_status_t st) {
  // Note: This function was written with regard to zx_ioports_request(),
  // but it may be a good idea to fill this out with more ZX_ statuses
  // if needed in the future.
  switch (st) {
    case ZX_ERR_NO_MEMORY:
      return AE_NO_MEMORY;
    case ZX_ERR_ACCESS_DENIED:
      return AE_ACCESS;
    case ZX_ERR_INVALID_ARGS:
      return AE_BAD_PARAMETER;
    case ZX_OK:
      return AE_OK;
    default:
      return AE_ERROR;
  }
}

/**
 * @brief Read a value from an input port.
 *
 * @param Address Hardware I/O port address to be read.
 * @param Value A pointer to a location where the data is to be returned.
 * @param Width The port width in bits, either 8, 16, or 32.
 *
 * @return Exception code that indicates success or reason for failure.
 */
ACPI_STATUS AcpiOsReadPort(ACPI_IO_ADDRESS Address, UINT32* Value, UINT32 Width) {
  if (Address > max_io_port) {
    return AE_BAD_PARAMETER;
  }

  uint16_t io_port = (uint16_t)Address;

  if (zx_status_t st = handle_port_permissions(io_port, Width); st != ZX_OK) {
    return zx_status_to_acpi_status(st);
  }

  switch (Width) {
    case 8:
      *Value = inp(io_port);
      break;
    case 16:
      *Value = inpw(io_port);
      break;
    case 32:
      *Value = inpd(io_port);
      break;
    default:
      return AE_BAD_PARAMETER;
  }
  return AE_OK;
}

/**
 * @brief Write a value to an output port.
 *
 * @param Address Hardware I/O port address where data is to be written.
 * @param Value The value to be written.
 * @param Width The port width in bits, either 8, 16, or 32.
 *
 * @return Exception code that indicates success or reason for failure.
 */
ACPI_STATUS AcpiOsWritePort(ACPI_IO_ADDRESS Address, UINT32 Value, UINT32 Width) {
  if (Address > max_io_port) {
    return AE_BAD_PARAMETER;
  }

  uint16_t io_port = (uint16_t)Address;

  if (zx_status_t st = handle_port_permissions(io_port, Width); st != ZX_OK) {
    return zx_status_to_acpi_status(st);
  }

  switch (Width) {
    case 8:
      outp(io_port, (uint8_t)Value);
      break;
    case 16:
      outpw(io_port, (uint16_t)Value);
      break;
    case 32:
      outpd(io_port, (uint32_t)Value);
      break;
    default:
      return AE_BAD_PARAMETER;
  }
  return AE_OK;
}

/**
 * @brief Read/Write a value from a PCI configuration register.
 *
 * @param PciId The full PCI configuration space address, consisting of a
 *        segment number, bus number, device number, and function number.
 * @param Register The PCI register address to be read from.
 * @param Value A pointer to a location where the data is to be returned.
 * @param Width The register width in bits, either 8, 16, 32, or 64.
 * @param Write Write or Read.
 *
 * @return Exception code that indicates success or reason for failure.
 */
static ACPI_STATUS AcpiOsReadWritePciConfiguration(ACPI_PCI_ID* PciId, UINT32 Register,
                                                   UINT64* Value, UINT32 Width, bool Write) {
  if (LOCAL_TRACE) {
    printf("ACPIOS: %s PCI Config %x:%x:%x:%x register %#x width %u\n", Write ? "write" : "read",
           PciId->Segment, PciId->Bus, PciId->Device, PciId->Function, Register, Width);
  }

  // Only segment 0 is supported for now
  if (PciId->Segment != 0) {
    printf("ACPIOS: read/write config, segment != 0 not supported.\n");
    return AE_ERROR;
  }

  // Check bounds of device and function offsets
  if (PciId->Device >= PCIE_MAX_DEVICES_PER_BUS ||
      PciId->Function >= PCIE_MAX_FUNCTIONS_PER_DEVICE) {
    printf("ACPIOS: device out of reasonable bounds.\n");
    return AE_ERROR;
  }

  // PCI config only supports up to 32 bit values
  if (Write && (*Value > UINT_MAX)) {
    printf("ACPIOS: read/write config, Value passed does not fit confg registers.\n");
  }

  // Clear higher bits before a read
  if (!Write) {
    *Value = 0;
  }

#if __x86_64__
  uint8_t bus = static_cast<uint8_t>(PciId->Bus);
  uint8_t dev = static_cast<uint8_t>(PciId->Device);
  uint8_t func = static_cast<uint8_t>(PciId->Function);
  uint8_t offset = static_cast<uint8_t>(Register);
  zx_status_t st;
#ifdef ENABLE_USER_PCI
  pci_bdf_t addr = {bus, dev, func};
  switch (Width) {
    case 8u:
      (Write) ? st = pci_pio_write8(addr, offset, static_cast<uint8_t>(*Value))
              : st = pci_pio_read8(addr, offset, reinterpret_cast<uint8_t*>(Value));
      break;
    case 16u:
      (Write) ? st = pci_pio_write16(addr, offset, static_cast<uint16_t>(*Value))
              : st = pci_pio_read16(addr, offset, reinterpret_cast<uint16_t*>(Value));
      break;
    // assume 32bit by default since 64 bit reads on IO ports are not a thing supported by the spec
    default:
      (Write) ? st = pci_pio_write32(addr, offset, static_cast<uint32_t>(*Value))
              : st = pci_pio_read32(addr, offset, reinterpret_cast<uint32_t*>(Value));
  }
#else
  st = zx_pci_cfg_pio_rw(root_resource_handle, bus, dev, func, offset,
                         reinterpret_cast<uint32_t*>(Value), static_cast<uint8_t>(Width), Write);

#endif  // ENABLE_USER_PCI
#ifdef ACPI_DEBUG_OUTPUT
  if (st != ZX_OK) {
    printf("ACPIOS: pci rw error: %d\n", st);
  }
#endif  // ACPI_DEBUG_OUTPUT
  return (st == ZX_OK) ? AE_OK : AE_ERROR;
#endif  // __x86_64__

  return AE_NOT_IMPLEMENTED;
}
/**
 * @brief Read a value from a PCI configuration register.
 *
 * @param PciId The full PCI configuration space address, consisting of a
 *        segment number, bus number, device number, and function number.
 * @param Register The PCI register address to be read from.
 * @param Value A pointer to a location where the data is to be returned.
 * @param Width The register width in bits, either 8, 16, 32, or 64.
 *
 * @return Exception code that indicates success or reason for failure.
 */
ACPI_STATUS AcpiOsReadPciConfiguration(ACPI_PCI_ID* PciId, UINT32 Register, UINT64* Value,
                                       UINT32 Width) {
  return AcpiOsReadWritePciConfiguration(PciId, Register, Value, Width, false);
}

/**
 * @brief Write a value to a PCI configuration register.
 *
 * @param PciId The full PCI configuration space address, consisting of a
 *        segment number, bus number, device number, and function number.
 * @param Register The PCI register address to be written to.
 * @param Value Data to be written.
 * @param Width The register width in bits, either 8, 16, or 32.
 *
 * @return Exception code that indicates success or reason for failure.
 */
ACPI_STATUS AcpiOsWritePciConfiguration(ACPI_PCI_ID* PciId, UINT32 Register, UINT64 Value,
                                        UINT32 Width) {
  return AcpiOsReadWritePciConfiguration(PciId, Register, &Value, Width, true);
}

/**
 * @brief A hook before writing sleep registers to enter the sleep state.
 *
 * @param Which sleep state to enter
 * @param Register A value
 * @param Register B value
 *
 * @return AE_CTRL_TERMINATE to skip further sleep register writes, otherwise AE_OK
 */

ACPI_STATUS AcpiOsEnterSleep(UINT8 SleepState, UINT32 RegaValue, UINT32 RegbValue) {
  /* The upstream ACPICA code expects that AcpiHwLegacySleep() is invoked with interrupts
   * disabled.  It requires this because the last steps of going to sleep is writing to a few
   * registers, flushing the caches (so we don't lose data if the caches are dropped), and then
   * writing to a register to enter the sleep.  If we were to take an interrupt after the cache
   * flush but before entering sleep, we could have inconsistent memory after waking up.*/

  /* In Fuchsia, ACPICA runs in usermode and we don't expose a mechanism for it to disable
   * interrupts.  For full shutdown (sleep state 5) this does not matter as any cache corruption
   * will be trumped by full power loss. Any other sleep state becomes forbidden. */
  if (SleepState == ACPI_STATE_S5) {
    return (AE_OK);
  } else {
    return (AE_ERROR);
  }
}

/**
 * @brief Formatted stream output.
 *
 * @param Format A standard print format string.
 * @param ...
 */
void ACPI_INTERNAL_VAR_XFACE AcpiOsPrintf(const char* Format, ...) {
  va_list argp;
  va_start(argp, Format);
  AcpiOsVprintf(Format, argp);
  va_end(argp);
}

/**
 * @brief Formatted stream output.
 *
 * @param Format A standard print format string.
 * @param Args A variable parameter list
 */
void AcpiOsVprintf(const char* Format, va_list Args) {
  // Only implement if ACPI_DEBUG_OUTPUT is defined, otherwise this causes
  // excess boot spew.
#ifdef ACPI_DEBUG_OUTPUT
  vprintf(Format, Args);
#endif
}

/**
 * @brief Get current value of the system timer
 *
 * @return The current value of the system timer in 100-ns units.
 */
UINT64 AcpiOsGetTimer() { return zx_clock_get_monotonic() / 100; }

/**
 * @brief Break to the debugger or display a breakpoint message.
 *
 * @param Function Signal to be sent to the host operating system.  Either
 *        ACPI_SIGNAL_FATAL or ACPI_SIGNAL_BREAKPOINT
 * @param Info Data associated with the signal; type depends on signal type.
 *
 * @return Exception code that indicates success or reason for failure.
 */
ACPI_STATUS AcpiOsSignal(UINT32 Function, void* Info) {
  UNIMPLEMENTED();
  return AE_OK;
}

/*
 * According to the the ACPI specification, section 5.2.10, the platform boot firmware aligns the
 * FACS (Firmware ACPI Control Structure) on a 64-byte boundary anywhere within the system’s
 * memory address space. This means we can assume the alignment when interacting with it.
 * Specifically we need to be able to manipulate the GlobalLock contained in the FACS table with
 * atomic operations, and these require aligned accesses.
 *
 * Although we know that the table will be aligned, to prevent the compiler from complaining we use
 * a wrapper struct to set the alignment attribute.
 */
struct AlignedFacs {
  ACPI_TABLE_FACS table;
} __attribute__((aligned(8)));

/* Setting the alignment should not have changed the size. */
static_assert(sizeof(AlignedFacs) == sizeof(ACPI_TABLE_FACS));

/* @brief Acquire the ACPI global lock
 *
 * Implementation for ACPI_ACQUIRE_GLOBAL_LOCK
 *
 * @param FacsPtr pointer to the FACS ACPI structure
 *
 * @return True if the lock was successfully acquired
 */
bool _acpica_acquire_global_lock(void* FacsPtr) {
  ZX_DEBUG_ASSERT(reinterpret_cast<uintptr_t>(FacsPtr) % 8 == 0);
  AlignedFacs* table = (AlignedFacs*)FacsPtr;
  uint32_t old_val, new_val, test_val;
  do {
    old_val = test_val = table->table.GlobalLock;
    new_val = old_val & ~ACPI_GLOCK_PENDING;
    // If the lock is owned, we'll mark it pending
    if (new_val & ACPI_GLOCK_OWNED) {
      new_val |= ACPI_GLOCK_PENDING;
    }
    new_val |= ACPI_GLOCK_OWNED;
    __atomic_compare_exchange_n(&table->table.GlobalLock, &old_val, new_val, false,
                                __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
  } while (old_val != test_val);

  /* If we're here, we either acquired the lock or marked it pending */
  return !(new_val & ACPI_GLOCK_PENDING);
}

/* @brief Release the ACPI global lock
 *
 * Implementation for ACPI_RELEASE_GLOBAL_LOCK
 *
 * @param FacsPtr pointer to the FACS ACPI structure
 *
 * @return True if there is someone waiting to acquire the lock
 */
bool _acpica_release_global_lock(void* FacsPtr) {
  // the FACS table is required to be 8 byte aligned, so sanity check with an assert but otherwise
  // we can just treat it as being aligned.
  ZX_DEBUG_ASSERT(reinterpret_cast<uintptr_t>(FacsPtr) % 8 == 0);
  AlignedFacs* table = (AlignedFacs*)FacsPtr;
  uint32_t old_val, new_val, test_val;
  do {
    old_val = test_val = table->table.GlobalLock;
    new_val = old_val & ~(ACPI_GLOCK_PENDING | ACPI_GLOCK_OWNED);
    __atomic_compare_exchange_n(&table->table.GlobalLock, &old_val, new_val, false,
                                __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
  } while (old_val != test_val);

  return !!(old_val & ACPI_GLOCK_PENDING);
}
