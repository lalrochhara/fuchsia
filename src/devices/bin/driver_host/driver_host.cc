// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "driver_host.h"

#include <dlfcn.h>
#include <fuchsia/device/manager/c/fidl.h>
#include <fuchsia/device/manager/llcpp/fidl.h>
#include <inttypes.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/receiver.h>
#include <lib/async/cpp/wait.h>
#include <lib/ddk/binding.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/driver.h>
#include <lib/fdio/fdio.h>
#include <lib/fidl/coding.h>
#include <lib/zx/debuglog.h>
#include <lib/zx/process.h>
#include <lib/zx/resource.h>
#include <lib/zx/vmo.h>
#include <lib/zxio/inception.h>
#include <lib/zxio/null.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <zircon/assert.h>
#include <zircon/dlfcn.h>
#include <zircon/process.h>
#include <zircon/processargs.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/log.h>

#include <memory>
#include <new>
#include <utility>
#include <vector>

#include <fbl/auto_lock.h>
#include <fbl/function.h>
#include <fbl/string_printf.h>

#include "async_loop_owned_rpc_handler.h"
#include "composite_device.h"
#include "connection_destroyer.h"
#include "device_controller_connection.h"
#include "env.h"
#include "fidl_txn.h"
#include "log.h"
#include "main.h"
#include "proxy_iostate.h"
#include "scheduler_profile.h"
#include "tracing.h"

namespace {

bool property_value_type_valid(uint32_t value_type) {
  return value_type > ZX_DEVICE_PROPERTY_VALUE_UNDEFINED &&
         value_type <= ZX_DEVICE_PROPERTY_VALUE_BOOL;
}

fuchsia_device_manager::wire::DeviceProperty convert_device_prop(const zx_device_prop_t& prop) {
  return fuchsia_device_manager::wire::DeviceProperty{
      .id = prop.id,
      .reserved = prop.reserved,
      .value = prop.value,
  };
}

fuchsia_device_manager::wire::DeviceStrProperty convert_device_str_prop(
    const zx_device_str_prop_t& prop, fidl::AnyAllocator& allocator) {
  ZX_ASSERT(property_value_type_valid(prop.property_value.value_type));

  auto str_property = fuchsia_device_manager::wire::DeviceStrProperty{
      .key = fidl::StringView(allocator, prop.key),
  };

  if (prop.property_value.value_type == ZX_DEVICE_PROPERTY_VALUE_INT) {
    str_property.value = fuchsia_device_manager::wire::PropertyValue::WithIntValue(
        fidl::ObjectView<uint32_t>(allocator, prop.property_value.value.int_val));
  } else if (prop.property_value.value_type == ZX_DEVICE_PROPERTY_VALUE_STRING) {
    str_property.value = fuchsia_device_manager::wire::PropertyValue::WithStrValue(
        fidl::ObjectView<fidl::StringView>(
            allocator, fidl::StringView(allocator, prop.property_value.value.str_val)));
  } else if (prop.property_value.value_type == ZX_DEVICE_PROPERTY_VALUE_BOOL) {
    str_property.value = fuchsia_device_manager::wire::PropertyValue::WithBoolValue(
        fidl::ObjectView<bool>(allocator, prop.property_value.value.bool_val));
  }

  return str_property;
}

static fx_log_severity_t log_min_severity(const char* name, const char* flag) {
  if (!strcasecmp(flag, "error")) {
    return FX_LOG_ERROR;
  }
  if (!strcasecmp(flag, "warning")) {
    return FX_LOG_WARNING;
  }
  if (!strcasecmp(flag, "info")) {
    return FX_LOG_INFO;
  }
  if (!strcasecmp(flag, "debug")) {
    return FX_LOG_DEBUG;
  }
  if (!strcasecmp(flag, "trace")) {
    return FX_LOG_TRACE;
  }
  if (!strcasecmp(flag, "serial")) {
    return DDK_LOG_SERIAL;
  }
  LOGF(WARNING, "Invalid minimum log severity '%s' for driver '%s', will log all", flag, name);
  return FX_LOG_ALL;
}

zx_status_t log_rpc_result(const fbl::RefPtr<zx_device_t>& dev, const char* opname,
                           zx_status_t status, zx_status_t call_status = ZX_OK) {
  if (status != ZX_OK) {
    constexpr char kLogFormat[] = "Failed %s RPC: %s";
    if (status == ZX_ERR_PEER_CLOSED) {
      // TODO(https://fxbug.dev/52627): change to an ERROR log once driver
      // manager can shut down gracefully.
      LOGD(WARNING, *dev, kLogFormat, opname, zx_status_get_string(status));
    } else {
      LOGD(ERROR, *dev, kLogFormat, opname, zx_status_get_string(status));
    }
    return status;
  }
  if (call_status != ZX_OK && call_status != ZX_ERR_NOT_FOUND) {
    LOGD(ERROR, *dev, "Failed %s: %s", opname, zx_status_get_string(call_status));
  }
  return call_status;
}

}  // namespace

const char* mkdevpath(const zx_device_t& dev, char* const path, size_t max) {
  if (max == 0) {
    return "";
  }
  char* end = path + max;
  char sep = 0;

  auto append_name = [&end, &path, &sep](const zx_device_t& dev) {
    *(--end) = sep;

    size_t len = strlen(dev.name());
    if (len > static_cast<size_t>(end - path)) {
      return;
    }
    end -= len;
    memcpy(end, dev.name(), len);
    sep = '/';
  };

  append_name(dev);

  fbl::RefPtr<zx_device> itr_dev = dev.parent();
  while (itr_dev && end > path) {
    append_name(*itr_dev);
    itr_dev = itr_dev->parent();
  }

  // If devpath is longer than |max|, add an ellipsis.
  constexpr char ellipsis[] = "...";
  constexpr size_t ellipsis_len = sizeof(ellipsis) - 1;
  if (*end == sep && max > ellipsis_len) {
    if (ellipsis_len > static_cast<size_t>(end - path)) {
      end = path;
    } else {
      end -= ellipsis_len;
    }
    memcpy(end, ellipsis, ellipsis_len);
  }

  return end;
}

zx_status_t zx_driver::Create(std::string_view libname, InspectNodeCollection& drivers,
                              fbl::RefPtr<zx_driver>* out_driver) {
  char process_name[ZX_MAX_NAME_LEN] = {};
  zx::process::self()->get_property(ZX_PROP_NAME, process_name, sizeof(process_name));
  const char* tags[] = {process_name, "driver"};
  fx_logger_config_t config{
      .min_severity = FX_LOG_SEVERITY_DEFAULT,
      .console_fd = getenv_bool("devmgr.log-to-debuglog", false) ? dup(STDOUT_FILENO) : -1,
      .log_service_channel = ZX_HANDLE_INVALID,
      .tags = tags,
      .num_tags = std::size(tags),
  };
  fx_logger_t* logger;
  zx_status_t status = fx_logger_create(&config, &logger);
  if (status != ZX_OK) {
    return status;
  }

  *out_driver = fbl::AdoptRef(new zx_driver(logger, libname, drivers));
  return ZX_OK;
}

zx_driver::zx_driver(fx_logger_t* logger, std::string_view libname, InspectNodeCollection& drivers)
    : logger_(logger), libname_(libname), inspect_(drivers, std::string(libname)) {}

zx_driver::~zx_driver() { fx_logger_destroy(logger_); }

zx_status_t DriverHostContext::SetupRootDevcoordinatorConnection(zx::channel ch) {
  auto conn = std::make_unique<internal::DevhostControllerConnection>(this);
  if (conn == nullptr) {
    return ZX_ERR_NO_MEMORY;
  }

  conn->set_channel(std::move(ch));
  return internal::DevhostControllerConnection::BeginWait(std::move(conn), loop_.dispatcher());
}

// Send message to driver_manager asking to add child device to
// parent device.  Called under the api lock.
zx_status_t DriverHostContext::DriverManagerAdd(const fbl::RefPtr<zx_device_t>& parent,
                                                const fbl::RefPtr<zx_device_t>& child,
                                                const char* proxy_args,
                                                const zx_device_prop_t* props, uint32_t prop_count,
                                                const zx_device_str_prop_t* str_props,
                                                uint32_t str_prop_count, zx::vmo inspect,
                                                zx::channel client_remote) {
  bool add_invisible = child->flags() & DEV_FLAG_INVISIBLE;
  using fuchsia_device_manager::wire::AddDeviceConfig;
  AddDeviceConfig add_device_config;

  if (child->flags() & DEV_FLAG_ALLOW_MULTI_COMPOSITE) {
    add_device_config |= AddDeviceConfig::kAllowMultiComposite;
  }
  if (add_invisible) {
    add_device_config |= AddDeviceConfig::kInvisible;
  }
  if (child->flags() & DEV_FLAG_UNBINDABLE) {
    add_device_config |= AddDeviceConfig::kSkipAutobind;
  }

  zx_status_t status;
  zx::channel coordinator_local, coordinator_remote;
  if ((status = zx::channel::create(0, &coordinator_local, &coordinator_remote)) != ZX_OK) {
    return status;
  }

  zx::channel device_controller, device_controller_remote;
  if ((status = zx::channel::create(0, &device_controller, &device_controller_remote)) != ZX_OK) {
    return status;
  }

  fidl::Client<fuchsia_device_manager::Coordinator> coordinator;
  coordinator.Bind(std::move(coordinator_local), loop_.dispatcher());
  std::unique_ptr<DeviceControllerConnection> conn;
  status = DeviceControllerConnection::Create(this, child, std::move(device_controller),
                                              std::move(coordinator), &conn);
  if (status != ZX_OK) {
    return status;
  }

  std::vector<fuchsia_device_manager::wire::DeviceProperty> props_list = {};
  for (size_t i = 0; i < prop_count; i++) {
    props_list.push_back(convert_device_prop(props[i]));
  }

  fidl::FidlAllocator allocator;
  std::vector<fuchsia_device_manager::wire::DeviceStrProperty> str_props_list = {};
  for (size_t i = 0; i < str_prop_count; i++) {
    if (!property_value_type_valid(str_props[i].property_value.value_type)) {
      return ZX_ERR_INVALID_ARGS;
    }
    str_props_list.push_back(convert_device_str_prop(str_props[i], allocator));
  }

  const auto& rpc = parent->coordinator_client;
  if (!rpc) {
    return ZX_ERR_IO_REFUSED;
  }
  size_t proxy_args_len = proxy_args ? strlen(proxy_args) : 0;
  zx_status_t call_status = ZX_OK;
  static_assert(sizeof(zx_device_prop_t) == sizeof(uint64_t));
  uint64_t device_id = 0;

  ::fuchsia_device_manager::wire::DevicePropertyList property_list = {
      .props = ::fidl::VectorView<fuchsia_device_manager::wire::DeviceProperty>::FromExternal(
          props_list),
      .str_props =
          ::fidl::VectorView<fuchsia_device_manager::wire::DeviceStrProperty>::FromExternal(
              str_props_list),
  };

  auto response = rpc->AddDevice_Sync(
      std::move(coordinator_remote), std::move(device_controller_remote), property_list,
      ::fidl::StringView::FromExternal(child->name()), child->protocol_id(),
      ::fidl::StringView::FromExternal(child->driver->libname()),
      ::fidl::StringView::FromExternal(proxy_args, proxy_args_len), add_device_config,
      child->ops()->init /* has_init */, std::move(inspect), std::move(client_remote));
  status = response.status();
  if (status == ZX_OK) {
    if (response.Unwrap()->result.is_response()) {
      device_id = response.Unwrap()->result.response().local_device_id;
      if (add_invisible) {
        // Mark child as invisible until the init function is replied.
        child->set_flag(DEV_FLAG_INVISIBLE);
      }
    } else {
      call_status = response.Unwrap()->result.err();
    }
  }

  status = log_rpc_result(parent, "add-device", status, call_status);
  if (status != ZX_OK) {
    return status;
  }

  child->set_local_id(device_id);
  return DeviceControllerConnection::BeginWait(std::move(conn), loop_.dispatcher());
}

// Send message to driver_manager informing it that this device
// is being removed.  Called under the api lock.
zx_status_t DriverHostContext::DriverManagerRemove(fbl::RefPtr<zx_device_t> dev) {
  DeviceControllerConnection* conn = dev->conn.load();
  if (conn == nullptr) {
    LOGD(ERROR, *dev, "Invalid device controller connection");
    return ZX_ERR_INTERNAL;
  }
  VLOGD(1, *dev, "Removing device %p", dev.get());

  // This must be done before the RemoveDevice message is sent to
  // driver_manager, since driver_manager will close the channel in response.
  // The async loop may see the channel close before it sees the queued
  // shutdown packet, so it needs to check if dev->conn has been nulled to
  // handle that gracefully.
  dev->conn.store(nullptr);

  // Drop the device vnode, since no one should be able to open connections anymore.
  // This will break the reference cycle between the DevfsVnode and the zx_device.
  dev->vnode.reset();

  // respond to the remove fidl call
  dev->removal_cb(ZX_OK);

  // Forget our local ID, to release the reference stored by the local ID map
  dev->set_local_id(0);

  // Forget about our rpc channel since after the port_queue below it may be
  // closed.
  dev->rpc = zx::unowned_channel();
  dev->coordinator_client = {};

  // queue an event to destroy the connection
  ConnectionDestroyer::Get()->QueueDeviceControllerConnection(loop_.dispatcher(), conn);

  // shut down our proxy rpc channel if it exists
  ProxyIosDestroy(dev);

  return ZX_OK;
}

void DriverHostContext::ProxyIosDestroy(const fbl::RefPtr<zx_device_t>& dev) {
  fbl::AutoLock guard(&dev->proxy_ios_lock);

  if (dev->proxy_ios) {
    dev->proxy_ios->CancelLocked(loop_.dispatcher());
  }
}

zx_status_t DriverHostContext::FindDriver(std::string_view libname, zx::vmo vmo,
                                          fbl::RefPtr<zx_driver_t>* out) {
  // check for already-loaded driver first
  for (auto& drv : drivers_) {
    if (!libname.compare(drv.libname())) {
      *out = fbl::RefPtr(&drv);
      return drv.status();
    }
  }

  fbl::RefPtr<zx_driver> new_driver;
  zx_status_t status = zx_driver::Create(libname, inspect().drivers(), &new_driver);
  if (status != ZX_OK) {
    return status;
  }

  // Let the |drivers_| list and our out parameter each have a refcount.
  drivers_.push_back(new_driver);
  *out = new_driver;

  const char* c_libname = new_driver->libname().c_str();

  void* dl = dlopen_vmo(vmo.get(), RTLD_NOW);
  if (dl == nullptr) {
    LOGF(ERROR, "Cannot load '%s': %s", c_libname, dlerror());
    new_driver->set_status(ZX_ERR_IO);
    return new_driver->status();
  }

  auto dn = static_cast<const zircon_driver_note_t*>(dlsym(dl, "__zircon_driver_note__"));
  if (dn == nullptr) {
    LOGF(ERROR, "Driver '%s' missing __zircon_driver_note__ symbol", c_libname);
    new_driver->set_status(ZX_ERR_IO);
    return new_driver->status();
  }
  auto ops = static_cast<const zx_driver_ops_t**>(dlsym(dl, "__zircon_driver_ops__"));
  auto dr = static_cast<zx_driver_rec_t*>(dlsym(dl, "__zircon_driver_rec__"));
  if (dr == nullptr) {
    LOGF(ERROR, "Driver '%s' missing __zircon_driver_rec__ symbol", c_libname);
    new_driver->set_status(ZX_ERR_IO);
    return new_driver->status();
  }
  // TODO(kulakowski) Eventually just check __zircon_driver_ops__,
  // when bind programs are standalone.
  if (ops == nullptr) {
    ops = &dr->ops;
  }
  if (!(*ops)) {
    LOGF(ERROR, "Driver '%s' has nullptr ops", c_libname);
    new_driver->set_status(ZX_ERR_INVALID_ARGS);
    return new_driver->status();
  }
  if ((*ops)->version != DRIVER_OPS_VERSION) {
    LOGF(ERROR, "Driver '%s' has bad driver ops version %#lx, expecting %#lx", c_libname,
         (*ops)->version, DRIVER_OPS_VERSION);
    new_driver->set_status(ZX_ERR_INVALID_ARGS);
    return new_driver->status();
  }

  new_driver->set_driver_rec(dr);
  new_driver->set_name(dn->payload.name);
  new_driver->set_ops(*ops);
  dr->driver = new_driver.get();

  // Check for minimum log severity of driver.
  const auto flag_name = fbl::StringPrintf("driver.%s.log", new_driver->name());
  const char* flag_value = getenv(flag_name.data());
  if (flag_value != nullptr) {
    fx_log_severity_t min_severity = log_min_severity(new_driver->name(), flag_value);
    status = fx_logger_set_min_severity(new_driver->logger(), min_severity);
    if (status != ZX_OK) {
      LOGF(ERROR, "Failed to set minimum log severity for driver '%s': %s", new_driver->name(),
           zx_status_get_string(status));
    } else {
      LOGF(INFO, "Driver '%s' set minimum log severity to %d", new_driver->name(), min_severity);
    }
  }

  if (new_driver->has_init_op()) {
    new_driver->set_status(new_driver->InitOp());
    if (new_driver->status() != ZX_OK) {
      LOGF(ERROR, "Driver '%s' failed in init: %s", c_libname,
           zx_status_get_string(new_driver->status()));
    }
  } else {
    new_driver->set_status(ZX_OK);
  }

  return new_driver->status();
}

namespace internal {

namespace {

// We need a global pointer to a DriverHostContext so that we can implement the functions exported
// to drivers.  Some of these functions unfortunately do not take an argument that can be used to
// find a context.
DriverHostContext* kContextForApi = nullptr;

}  // namespace

void RegisterContextForApi(DriverHostContext* context) {
  ZX_ASSERT((context == nullptr) != (kContextForApi == nullptr));
  kContextForApi = context;
}
DriverHostContext* ContextForApi() { return kContextForApi; }

void DevhostControllerConnection::CreateDevice(CreateDeviceRequestView request,
                                               CreateDeviceCompleter::Sync& completer) {
  std::string_view driver_path(request->driver_path.data(), request->driver_path.size());
  // This does not operate under the driver_host api lock,
  // since the newly created device is not visible to
  // any API surface until a driver is bound to it.
  // (which can only happen via another message on this thread)

  // named driver -- ask it to create the device
  fbl::RefPtr<zx_driver_t> drv;
  zx_status_t r = driver_host_context_->FindDriver(driver_path, std::move(request->driver), &drv);
  if (r != ZX_OK) {
    LOGF(ERROR, "Failed to load driver '%.*s': %s", static_cast<int>(driver_path.size()),
         driver_path.data(), zx_status_get_string(r));
    return;
  }
  if (!drv->has_create_op()) {
    LOGF(ERROR, "Driver does not support create operation");
    return;
  }

  fidl::Client<fuchsia_device_manager::Coordinator> coordinator;
  coordinator.Bind(std::move(request->coordinator_rpc), driver_host_context_->loop().dispatcher());

  // Create a dummy parent device for use in this call to Create
  fbl::RefPtr<zx_device> parent;
  r = zx_device::Create(driver_host_context_, "device_create dummy", drv.get(), &parent);
  if (r != ZX_OK) {
    LOGF(ERROR, "Failed to create device: %s", zx_status_get_string(r));
    return;
  }
  // magic cookie for device create handshake
  CreationContext creation_context = {
      .parent = std::move(parent),
      .child = nullptr,
      .device_controller_rpc = zx::unowned_channel(request->device_controller_rpc.channel()),
      .coordinator_client = coordinator.Clone(),
  };

  r = drv->CreateOp(&creation_context, creation_context.parent, "proxy", request->proxy_args.data(),
                    request->parent_proxy.release());

  // Suppress a warning about dummy device being in a bad state.  The
  // message is spurious in this case, since the dummy parent never
  // actually begins its device lifecycle.  This flag is ordinarily
  // set by device_remove().
  creation_context.parent->set_flag(DEV_FLAG_DEAD);

  if (r != ZX_OK) {
    constexpr char kLogFormat[] = "Failed to create driver: %s";
    if (r == ZX_ERR_PEER_CLOSED) {
      // TODO(https://fxbug.dev/52627): change to an ERROR log once driver
      // manager can shut down gracefully.
      LOGF(WARNING, kLogFormat, zx_status_get_string(r));
    } else {
      LOGF(ERROR, kLogFormat, zx_status_get_string(r));
    }
    return;
  }

  auto new_device = std::move(creation_context.child);
  if (new_device == nullptr) {
    LOGF(ERROR, "Driver did not create a device");
    return;
  }

  new_device->set_local_id(request->local_device_id);
  std::unique_ptr<DeviceControllerConnection> newconn;
  r = DeviceControllerConnection::Create(driver_host_context_, std::move(new_device),
                                         request->device_controller_rpc.TakeChannel(),
                                         std::move(coordinator), &newconn);
  if (r != ZX_OK) {
    return;
  }

  // TODO: inform devcoord
  VLOGF(1, "Created device %p '%.*s'", new_device.get(), static_cast<int>(driver_path.size()),
        driver_path.data());
  r = DeviceControllerConnection::BeginWait(std::move(newconn),
                                            driver_host_context_->loop().dispatcher());
  if (r != ZX_OK) {
    LOGF(ERROR, "Failed to wait for device controller connection: %s", zx_status_get_string(r));
    return;
  }
}

void DevhostControllerConnection::CreateCompositeDevice(
    CreateCompositeDeviceRequestView request, CreateCompositeDeviceCompleter::Sync& completer) {
  // Convert the fragment IDs into zx_device references
  CompositeFragments fragments_list(new CompositeFragment[request->fragments.count()],
                                    request->fragments.count());
  {
    // Acquire the API lock so that we don't have to worry about concurrent
    // device removes
    fbl::AutoLock lock(&driver_host_context_->api_lock());

    for (size_t i = 0; i < request->fragments.count(); ++i) {
      const auto& fragment = request->fragments.data()[i];
      uint64_t local_id = fragment.id;
      fbl::RefPtr<zx_device_t> dev = zx_device::GetDeviceFromLocalId(local_id);
      if (dev == nullptr || (dev->flags() & DEV_FLAG_DEAD)) {
        completer.Reply(ZX_ERR_NOT_FOUND);
        return;
      }
      fragments_list[i].name = std::string(fragment.name.data(), fragment.name.size());
      fragments_list[i].device = std::move(dev);
    }
  }

  auto driver = GetCompositeDriver(driver_host_context_);
  if (driver == nullptr) {
    completer.Reply(ZX_ERR_INTERNAL);
    return;
  }

  fbl::RefPtr<zx_device_t> dev;
  static_assert(fuchsia_device_manager_DEVICE_NAME_MAX + 1 >= sizeof(dev->name()));
  zx_status_t status = zx_device::Create(driver_host_context_,
                                         std::string(request->name.data(), request->name.size()),
                                         driver.get(), &dev);
  if (status != ZX_OK) {
    completer.Reply(status);
    return;
  }
  dev->set_local_id(request->local_device_id);

  fidl::Client<fuchsia_device_manager::Coordinator> coordinator;
  coordinator.Bind(std::move(request->coordinator_rpc), driver_host_context_->loop().dispatcher());
  std::unique_ptr<DeviceControllerConnection> newconn;
  status = DeviceControllerConnection::Create(driver_host_context_, dev,
                                              request->device_controller_rpc.TakeChannel(),
                                              std::move(coordinator), &newconn);
  if (status != ZX_OK) {
    completer.Reply(status);
    return;
  }

  status = InitializeCompositeDevice(dev, std::move(fragments_list));
  if (status != ZX_OK) {
    completer.Reply(status);
    return;
  }

  VLOGF(1, "Created composite device %p '%s'", dev.get(), dev->name());
  status = DeviceControllerConnection::BeginWait(std::move(newconn),
                                                 driver_host_context_->loop().dispatcher());
  if (status != ZX_OK) {
    completer.Reply(status);
    return;
  }
  completer.Reply(ZX_OK);
}

void DevhostControllerConnection::CreateDeviceStub(CreateDeviceStubRequestView request,
                                                   CreateDeviceStubCompleter::Sync& completer) {
  // This method is used for creating driverless proxies in case of misc, root, test devices.
  // Since there are no proxy drivers backing the device, a dummy proxy driver will be used for
  // device creation.
  if (!proxy_driver_) {
    auto status =
        zx_driver::Create("proxy", driver_host_context_->inspect().drivers(), &proxy_driver_);
    if (status != ZX_OK) {
      return;
    }
  }

  fbl::RefPtr<zx_device_t> dev;
  zx_status_t r = zx_device::Create(driver_host_context_, "proxy", proxy_driver_.get(), &dev);
  // TODO: dev->ops() and other lifecycle bits
  // no name means a dummy proxy device
  if (r != ZX_OK) {
    return;
  }
  dev->set_protocol_id(request->protocol_id);
  dev->set_ops(&kDeviceDefaultOps);
  dev->set_local_id(request->local_device_id);

  fidl::Client<fuchsia_device_manager::Coordinator> coordinator;
  coordinator.Bind(std::move(request->coordinator_rpc), driver_host_context_->loop().dispatcher());
  std::unique_ptr<DeviceControllerConnection> newconn;
  r = DeviceControllerConnection::Create(driver_host_context_, dev,
                                         request->device_controller_rpc.TakeChannel(),
                                         std::move(coordinator), &newconn);
  if (r != ZX_OK) {
    return;
  }
  VLOGF(1, "Created device stub %p '%s'", dev.get(), dev->name());
  r = DeviceControllerConnection::BeginWait(std::move(newconn),
                                            driver_host_context_->loop().dispatcher());
  if (r != ZX_OK) {
    return;
  }
}

// TODO(fxbug.dev/68309): Implement Restart.
void DevhostControllerConnection::Restart(RestartRequestView request,
                                          RestartCompleter::Sync& completer) {
  completer.Reply(ZX_OK);
}

zx_status_t DevhostControllerConnection::HandleRead() {
  zx::unowned_channel conn = channel();
  uint8_t msg[ZX_CHANNEL_MAX_MSG_BYTES];
  zx_handle_info_t hin[ZX_CHANNEL_MAX_MSG_HANDLES];
  uint32_t msize = sizeof(msg);
  uint32_t hcount = std::size(hin);
  zx_status_t status = conn->read_etc(0, msg, hin, msize, hcount, &msize, &hcount);
  if (status != ZX_OK) {
    return status;
  }

  fidl_incoming_msg_t fidl_msg = {
      .bytes = msg,
      .handles = hin,
      .num_bytes = msize,
      .num_handles = hcount,
  };

  if (fidl_msg.num_bytes < sizeof(fidl_message_header_t)) {
    FidlHandleInfoCloseMany(fidl_msg.handles, fidl_msg.num_handles);
    return ZX_ERR_IO;
  }

  auto hdr = static_cast<fidl_message_header_t*>(fidl_msg.bytes);
  DevmgrFidlTxn txn(std::move(conn), hdr->txid);
  fidl::WireDispatch<fuchsia_device_manager::DevhostController>(
      this, fidl::IncomingMessage::FromEncodedCMessage(&fidl_msg), &txn);
  return txn.Status();
}

// handles devcoordinator rpc

void DevhostControllerConnection::HandleRpc(std::unique_ptr<DevhostControllerConnection> conn,
                                            async_dispatcher_t* dispatcher, async::WaitBase* wait,
                                            zx_status_t status, const zx_packet_signal_t* signal) {
  if (status != ZX_OK) {
    LOGF(ERROR, "Failed to wait on %p from driver_manager: %s", conn.get(),
         zx_status_get_string(status));
    return;
  }
  if (signal->observed & ZX_CHANNEL_READABLE) {
    status = conn->HandleRead();
    if (status != ZX_OK) {
      LOGF(FATAL, "Unhandled RPC on %p from driver_manager: %s", conn.get(),
           zx_status_get_string(status));
    }
    BeginWait(std::move(conn), dispatcher);
    return;
  }
  if (signal->observed & ZX_CHANNEL_PEER_CLOSED) {
    // This is expected in test environments where driver_manager has terminated.
    // TODO(fxbug.dev/52627): Support graceful termination.
    LOGF(WARNING, "Disconnected %p from driver_manager", conn.get());
    zx_process_exit(1);
  }
  LOGF(WARNING, "Unexpected signal state %#08x", signal->observed);
  BeginWait(std::move(conn), dispatcher);
}

int main(int argc, char** argv) {
  char process_name[ZX_MAX_NAME_LEN] = {};
  zx::process::self()->get_property(ZX_PROP_NAME, process_name, sizeof(process_name));
  const char* tags[] = {process_name, "device"};
  fx_logger_config_t config{
      .min_severity = getenv_bool("devmgr.verbose", false) ? FX_LOG_ALL : FX_LOG_SEVERITY_DEFAULT,
      .console_fd = getenv_bool("devmgr.log-to-debuglog", false) ? dup(STDOUT_FILENO) : -1,
      .log_service_channel = ZX_HANDLE_INVALID,
      .tags = tags,
      .num_tags = std::size(tags),
  };
  zx_status_t status = fx_log_reconfigure(&config);
  if (status != ZX_OK) {
    return status;
  }

  zx::resource root_resource(zx_take_startup_handle(PA_HND(PA_RESOURCE, 0)));
  if (!root_resource.is_valid()) {
    LOGF(WARNING, "No root resource handle");
  }

  zx::channel root_conn_channel(zx_take_startup_handle(PA_HND(PA_USER0, 0)));
  if (!root_conn_channel.is_valid()) {
    LOGF(ERROR, "Invalid root connection to driver_manager");
    return ZX_ERR_BAD_HANDLE;
  }

  DriverHostContext ctx(&kAsyncLoopConfigAttachToCurrentThread, std::move(root_resource));

  const char* root_driver_path = getenv("devmgr.root_driver_path");
  if (root_driver_path != nullptr) {
    ctx.set_root_driver_path(root_driver_path);
  }

  RegisterContextForApi(&ctx);

  status = connect_scheduler_profile_provider();
  if (status != ZX_OK) {
    LOGF(INFO, "Failed to connect to profile provider: %s", zx_status_get_string(status));
    return status;
  }

  if (getenv_bool("driver.tracing.enable", true)) {
    status = start_trace_provider();
    if (status != ZX_OK) {
      LOGF(INFO, "Failed to register trace provider: %s", zx_status_get_string(status));
      // This is not a fatal error.
    }
  }
  auto stop_tracing = fit::defer([]() { stop_trace_provider(); });

  status = ctx.SetupRootDevcoordinatorConnection(std::move(root_conn_channel));
  if (status != ZX_OK) {
    LOGF(ERROR, "Failed to watch root connection to driver_manager: %s",
         zx_status_get_string(status));
    return status;
  }

  status = ctx.inspect().Serve(zx::channel(zx_take_startup_handle(PA_DIRECTORY_REQUEST)),
                               ctx.loop().dispatcher());
  if (status != ZX_OK) {
    LOGF(WARNING, "driver_host: error serving diagnostics directory: %s\n",
         zx_status_get_string(status));
    // This is not a fatal error
  }

  return ctx.loop().Run(zx::time::infinite(), false /* once */);
}

}  // namespace internal

void DriverHostContext::MakeVisible(const fbl::RefPtr<zx_device_t>& dev,
                                    const device_make_visible_args_t* args) {
  ZX_ASSERT_MSG(!dev->ops()->init,
                "Cannot call device_make_visible if init hook is implemented."
                "The device will automatically be made visible once the init hook is replied to.");
  const auto& client = dev->coordinator_client;
  if (!client) {
    return;
  }

  if (args && args->power_states && args->power_state_count != 0) {
    dev->SetPowerStates(args->power_states, args->power_state_count);
  }
  if (args && args->performance_states && (args->performance_state_count != 0)) {
    dev->SetPerformanceStates(args->performance_states, args->performance_state_count);
  }

  // TODO(teisenbe): Handle failures here...
  VLOGD(1, *dev, "make-visible");
  auto response = client->MakeVisible_Sync();
  zx_status_t status = response.status();
  zx_status_t call_status = ZX_OK;
  if (status == ZX_OK) {
    if (response.Unwrap()->result.is_err()) {
      call_status = response.Unwrap()->result.err();
    }
  }
  log_rpc_result(dev, "make-visible", status, call_status);
  dev->unset_flag(DEV_FLAG_INVISIBLE);

  // Reply to any pending bind/rebind requests, if all
  // the children are initialized.
  bool reply_bind_rebind = true;
  for (auto& child : dev->parent()->children()) {
    if (child.flags() & DEV_FLAG_INVISIBLE) {
      reply_bind_rebind = false;
    }
  }
  if (!reply_bind_rebind || !dev->parent()->complete_bind_rebind_after_init()) {
    return;
  }
  status = (status == ZX_OK) ? call_status : status;
  if (auto bind_conn = dev->parent()->take_bind_conn(); bind_conn) {
    bind_conn(status);
  }
  if (auto rebind_conn = dev->parent()->take_rebind_conn(); rebind_conn) {
    rebind_conn(status);
  }
}

zx_status_t DriverHostContext::ScheduleRemove(const fbl::RefPtr<zx_device_t>& dev,
                                              bool unbind_self) {
  const auto& client = dev->coordinator_client;
  ZX_ASSERT(client);
  VLOGD(1, *dev, "schedule-remove");
  auto resp = client->ScheduleRemove(unbind_self);
  log_rpc_result(dev, "schedule-remove", resp.status());
  return resp.status();
}

zx_status_t DriverHostContext::ScheduleUnbindChildren(const fbl::RefPtr<zx_device_t>& dev) {
  const auto& client = dev->coordinator_client;
  ZX_ASSERT(client);
  VLOGD(1, *dev, "schedule-unbind-children");
  auto resp = client->ScheduleUnbindChildren();
  log_rpc_result(dev, "schedule-unbind-children", resp.status());
  return resp.status();
}

zx_status_t DriverHostContext::GetTopoPath(const fbl::RefPtr<zx_device_t>& dev, char* path,
                                           size_t max, size_t* actual) {
  fbl::RefPtr<zx_device_t> remote_dev = dev;
  if (dev->flags() & DEV_FLAG_INSTANCE) {
    // Instances cannot be opened a second time. If dev represents an instance, return the path
    // to its parent, prefixed with an '@'.
    if (max < 1) {
      return ZX_ERR_BUFFER_TOO_SMALL;
    }
    path[0] = '@';
    path++;
    max--;
    remote_dev = dev->parent();
  }

  const auto& client = remote_dev->coordinator_client;
  if (!client) {
    return ZX_ERR_IO_REFUSED;
  }

  VLOGD(1, *remote_dev, "get-topo-path");
  auto response = client->GetTopologicalPath_Sync();
  zx_status_t status = response.status();
  zx_status_t call_status = ZX_OK;
  if (status == ZX_OK) {
    if (response.Unwrap()->result.is_err()) {
      call_status = response.Unwrap()->result.err();
    } else {
      auto& r = response.Unwrap()->result.response();
      memcpy(path, r.path.data(), r.path.size());
      *actual = r.path.size();
    }
  }

  log_rpc_result(dev, "get-topo-path", status, call_status);
  if (status != ZX_OK) {
    return status;
  }
  if (call_status != ZX_OK) {
    return status;
  }

  path[*actual] = 0;
  *actual += 1;

  // Account for the prefixed '@' we may have added above.
  if (dev->flags() & DEV_FLAG_INSTANCE) {
    *actual += 1;
  }
  return ZX_OK;
}

zx_status_t DriverHostContext::DeviceBind(const fbl::RefPtr<zx_device_t>& dev,
                                          const char* drv_libname) {
  const auto& client = dev->coordinator_client;
  if (!client) {
    return ZX_ERR_IO_REFUSED;
  }
  VLOGD(1, *dev, "bind-device");
  auto driver_path = ::fidl::StringView::FromExternal(drv_libname);
  auto response = client->BindDevice_Sync(std::move(driver_path));
  zx_status_t status = response.status();
  zx_status_t call_status = ZX_OK;
  if (status == ZX_OK && response.Unwrap()->result.is_err()) {
    call_status = response.Unwrap()->result.err();
  }
  log_rpc_result(dev, "bind-device", status, call_status);
  if (status != ZX_OK) {
    return status;
  }

  return call_status;
}

zx_status_t DriverHostContext::DeviceRunCompatibilityTests(const fbl::RefPtr<zx_device_t>& dev,
                                                           int64_t hook_wait_time) {
  const auto& client = dev->coordinator_client;
  if (!client) {
    return ZX_ERR_IO_REFUSED;
  }
  VLOGD(1, *dev, "run-compatibility-test");
  auto response = client->RunCompatibilityTests_Sync(hook_wait_time);
  zx_status_t status = response.status();
  zx_status_t call_status = ZX_OK;
  if (status == ZX_OK && response.Unwrap()->result.is_err()) {
    call_status = response.Unwrap()->result.err();
  }
  log_rpc_result(dev, "run-compatibility-test", status, call_status);
  if (status != ZX_OK) {
    return status;
  }
  return call_status;
}

zx_status_t DriverHostContext::LoadFirmware(const zx_driver_t* drv,
                                            const fbl::RefPtr<zx_device_t>& dev, const char* path,
                                            zx_handle_t* vmo_handle, size_t* size) {
  if ((vmo_handle == nullptr) || (size == nullptr)) {
    return ZX_ERR_INVALID_ARGS;
  }

  zx::vmo vmo;
  const auto& client = dev->coordinator_client;
  if (!client) {
    return ZX_ERR_IO_REFUSED;
  }
  VLOGD(1, *dev, "load-firmware");
  auto drv_libname = ::fidl::StringView::FromExternal(drv->libname());
  auto str_path = ::fidl::StringView::FromExternal(path);
  auto response = client->LoadFirmware_Sync(std::move(drv_libname), std::move(str_path));
  zx_status_t status = response.status();
  zx_status_t call_status = ZX_OK;
  auto result = std::move(response.Unwrap()->result);
  if (result.is_err()) {
    call_status = result.err();
  } else {
    auto resp = std::move(result.mutable_response());
    *size = resp.size;
    vmo = std::move(resp.vmo);
  }
  log_rpc_result(dev, "load-firmware", status, call_status);
  if (status != ZX_OK) {
    return status;
  }
  *vmo_handle = vmo.release();
  if (call_status == ZX_OK && *vmo_handle == ZX_HANDLE_INVALID) {
    return ZX_ERR_INTERNAL;
  }
  return call_status;
}

void DriverHostContext::LoadFirmwareAsync(const zx_driver_t* drv,
                                          const fbl::RefPtr<zx_device_t>& dev, const char* path,
                                          load_firmware_callback_t callback, void* context) {
  ZX_DEBUG_ASSERT(callback);

  fbl::RefPtr<zx_device_t> device_ref = dev;

  const auto& client = dev->coordinator_client;
  if (!client) {
    callback(context, ZX_ERR_IO_REFUSED, ZX_HANDLE_INVALID, 0);
    return;
  }
  VLOGD(1, *dev, "load-firmware-async");
  auto drv_libname = ::fidl::StringView::FromExternal(drv->libname());
  auto str_path = ::fidl::StringView::FromExternal(path);
  auto result = client->LoadFirmware(
      std::move(drv_libname), std::move(str_path),
      [callback, context, dev = std::move(device_ref)](
          fidl::WireResponse<fuchsia_device_manager::Coordinator::LoadFirmware>* response) {
        zx_status_t call_status = ZX_OK;
        size_t size = 0;
        zx::vmo vmo;

        if (response->result.is_err()) {
          call_status = response->result.err();
        } else {
          auto& resp = response->result.mutable_response();
          size = resp.size;
          vmo = std::move(resp.vmo);
        }
        log_rpc_result(dev, "load-firmware-async", ZX_OK, call_status);
        if (call_status == ZX_OK && !vmo.is_valid()) {
          call_status = ZX_ERR_INTERNAL;
        }

        callback(context, call_status, vmo.release(), size);
      });

  if (result.status() != ZX_OK) {
    log_rpc_result(dev, "load-firmware-async", result.status(), ZX_OK);
    callback(context, result.status(), ZX_HANDLE_INVALID, 0);
  }
}

zx_status_t DriverHostContext::GetMetadata(const fbl::RefPtr<zx_device_t>& dev, uint32_t type,
                                           void* buf, size_t buflen, size_t* actual) {
  if (!buf) {
    return ZX_ERR_INVALID_ARGS;
  }

  const auto& client = dev->coordinator_client;
  if (!client) {
    return ZX_ERR_IO_REFUSED;
  }
  VLOGD(1, *dev, "get-metadata");
  auto response = client->GetMetadata_Sync(type);
  zx_status_t status = response.status();
  zx_status_t call_status = ZX_OK;
  if (status == ZX_OK) {
    if (response->result.is_response()) {
      const auto& r = response.Unwrap()->result.mutable_response();
      if (r.data.count() > buflen) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      memcpy(buf, r.data.data(), r.data.count());
      if (actual != nullptr) {
        *actual = r.data.count();
      }
    } else {
      call_status = response->result.err();
    }
  }
  return log_rpc_result(dev, "get-metadata", status, call_status);
}

zx_status_t DriverHostContext::GetMetadataSize(const fbl::RefPtr<zx_device_t>& dev, uint32_t type,
                                               size_t* out_length) {
  const auto& client = dev->coordinator_client;
  if (!client) {
    return ZX_ERR_IO_REFUSED;
  }
  VLOGD(1, *dev, "get-metadata-size");
  auto response = client->GetMetadataSize_Sync(type);
  zx_status_t status = response.status();
  zx_status_t call_status = ZX_OK;
  if (status == ZX_OK) {
    if (response->result.is_response()) {
      *out_length = response->result.response().size;
    } else {
      call_status = response->result.err();
    }
  }
  return log_rpc_result(dev, "get-metadata-size", status, call_status);
}

zx_status_t DriverHostContext::AddMetadata(const fbl::RefPtr<zx_device_t>& dev, uint32_t type,
                                           const void* data, size_t length) {
  if (!data && length) {
    return ZX_ERR_INVALID_ARGS;
  }
  const auto& client = dev->coordinator_client;
  if (!client) {
    return ZX_ERR_IO_REFUSED;
  }
  VLOGD(1, *dev, "add-metadata");
  auto response = client->AddMetadata_Sync(
      type, ::fidl::VectorView<uint8_t>::FromExternal(
                reinterpret_cast<uint8_t*>(const_cast<void*>(data)), length));
  zx_status_t status = response.status();
  zx_status_t call_status = ZX_OK;
  if (status == ZX_OK && response->result.is_err()) {
    call_status = response->result.err();
  }
  return log_rpc_result(dev, "add-metadata", status, call_status);
}

zx_status_t DriverHostContext::PublishMetadata(const fbl::RefPtr<zx_device_t>& dev,
                                               const char* path, uint32_t type, const void* data,
                                               size_t length) {
  if (!path || (!data && length)) {
    return ZX_ERR_INVALID_ARGS;
  }
  const auto& client = dev->coordinator_client;
  if (!client) {
    return ZX_ERR_IO_REFUSED;
  }
  VLOGD(1, *dev, "publish-metadata");
  auto response = client->PublishMetadata_Sync(
      ::fidl::StringView::FromExternal(path), type,
      ::fidl::VectorView<uint8_t>::FromExternal(reinterpret_cast<uint8_t*>(const_cast<void*>(data)),
                                                length));
  zx_status_t status = response.status();
  zx_status_t call_status = ZX_OK;
  if (status == ZX_OK && response->result.is_err()) {
    call_status = response->result.err();
  }
  return log_rpc_result(dev, "publish-metadata", status, call_status);
}

zx_status_t DriverHostContext::DeviceAddComposite(const fbl::RefPtr<zx_device_t>& dev,
                                                  const char* name,
                                                  const composite_device_desc_t* comp_desc) {
  if (comp_desc == nullptr || (comp_desc->props == nullptr && comp_desc->props_count > 0) ||
      comp_desc->fragments == nullptr || name == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  const auto& client = dev->coordinator_client;
  if (!client) {
    return ZX_ERR_IO_REFUSED;
  }

  VLOGD(1, *dev, "create-composite");
  std::vector<fuchsia_device_manager::wire::DeviceFragment> compvec = {};
  for (size_t i = 0; i < comp_desc->fragments_count; i++) {
    ::fidl::Array<fuchsia_device_manager::wire::DeviceFragmentPart,
                  fuchsia_device_manager::wire::kDeviceFragmentPartsMax>
        parts{};
    for (uint32_t j = 0; j < comp_desc->fragments[i].parts_count; j++) {
      ::fidl::Array<fuchsia_device_manager::wire::BindInstruction,
                    fuchsia_device_manager::wire::kDeviceFragmentPartInstructionsMax>
          bind_instructions{};
      for (uint32_t k = 0; k < comp_desc->fragments[i].parts[j].instruction_count; k++) {
        bind_instructions[k] = fuchsia_device_manager::wire::BindInstruction{
            .op = comp_desc->fragments[i].parts[j].match_program[k].op,
            .arg = comp_desc->fragments[i].parts[j].match_program[k].arg,
            .debug = comp_desc->fragments[i].parts[j].match_program[k].debug,
        };
      }
      auto part = fuchsia_device_manager::wire::DeviceFragmentPart{
          .match_program_count = comp_desc->fragments[i].parts[j].instruction_count,
          .match_program = bind_instructions,
      };
      parts[j] = part;
    }
    auto dc = fuchsia_device_manager::wire::DeviceFragment{
        .name = ::fidl::StringView::FromExternal(comp_desc->fragments[i].name,
                                                 strnlen(comp_desc->fragments[i].name, 32)),
        .parts_count = comp_desc->fragments[i].parts_count,
        .parts = parts,
    };
    compvec.push_back(std::move(dc));
  }

  std::vector<fuchsia_device_manager::wire::DeviceMetadata> metadata = {};
  for (size_t i = 0; i < comp_desc->metadata_count; i++) {
    auto meta = fuchsia_device_manager::wire::DeviceMetadata{
        .key = comp_desc->metadata_list[i].type,
        .data = fidl::VectorView<uint8_t>::FromExternal(
            reinterpret_cast<uint8_t*>(const_cast<void*>(comp_desc->metadata_list[i].data)),
            comp_desc->metadata_list[i].length)};
    metadata.emplace_back(std::move(meta));
  }

  std::vector<fuchsia_device_manager::wire::DeviceProperty> props = {};
  for (size_t i = 0; i < comp_desc->props_count; i++) {
    props.push_back(convert_device_prop(comp_desc->props[i]));
  }

  fuchsia_device_manager::wire::CompositeDeviceDescriptor comp_dev = {
      .props =
          ::fidl::VectorView<fuchsia_device_manager::wire::DeviceProperty>::FromExternal(props),
      .fragments =
          ::fidl::VectorView<fuchsia_device_manager::wire::DeviceFragment>::FromExternal(compvec),
      .coresident_device_index = comp_desc->coresident_device_index,
      .metadata =
          ::fidl::VectorView<fuchsia_device_manager::wire::DeviceMetadata>::FromExternal(metadata)};

  static_assert(sizeof(comp_desc->props[0]) == sizeof(uint64_t));
  auto response =
      client->AddCompositeDevice_Sync(::fidl::StringView::FromExternal(name), std::move(comp_dev));
  zx_status_t status = response.status();
  zx_status_t call_status = ZX_OK;
  if (status == ZX_OK && response->result.is_err()) {
    call_status = response->result.err();
  }
  return log_rpc_result(dev, "create-composite", status, call_status);
}
