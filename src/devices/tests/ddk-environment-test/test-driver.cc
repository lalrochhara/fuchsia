// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/device/environment/test/llcpp/fidl.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/driver.h>
#include <lib/ddk/platform-defs.h>
#include <zircon/errors.h>

#include <vector>

#include <ddktl/device.h>
#include <ddktl/fidl.h>
#include <fbl/alloc_checker.h>

#include "src/devices/tests/ddk-environment-test/test-environment-bind.h"
#include "src/lib/files/glob.h"

namespace {

using fuchsia_device_environment_test::TestDevice;

class TestEnvironmentDriver;
using DeviceType =
    ddk::Device<TestEnvironmentDriver, ddk::Unbindable, ddk::Messageable<TestDevice>::Mixin>;

class TestEnvironmentDriver : public DeviceType {
 public:
  explicit TestEnvironmentDriver(zx_device_t* parent) : DeviceType(parent) {}
  ~TestEnvironmentDriver() {}

  zx_status_t Bind() { return DdkAdd("ddk-environment-test"); }

  // Device protocol implementation.
  void DdkUnbind(ddk::UnbindTxn txn) { txn.Reply(); }
  void DdkRelease() { delete this; }

  // Device message ops implementation.
  void GetServiceList(GetServiceListRequestView request,
                      GetServiceListCompleter::Sync& completer) override;
};

void TestEnvironmentDriver::GetServiceList(GetServiceListRequestView request,
                                           GetServiceListCompleter::Sync& completer) {
  files::Glob glob("/svc/*");
  std::vector<fidl::StringView> services;
  for (const char* file : glob) {
    services.push_back(fidl::StringView::FromExternal(file));
  }
  completer.Reply(fidl::VectorView<fidl::StringView>::FromExternal(services));
}

zx_status_t TestEnvironmentBind(void* ctx, zx_device_t* device) {
  fbl::AllocChecker ac;
  auto dev = fbl::make_unique_checked<TestEnvironmentDriver>(&ac, device);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  auto status = dev->Bind();
  if (status == ZX_OK) {
    // devmgr is now in charge of the memory for dev
    __UNUSED auto ptr = dev.release();
  }
  return status;
}

zx_driver_ops_t driver_ops = []() -> zx_driver_ops_t {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = TestEnvironmentBind;
  return ops;
}();

}  // namespace

ZIRCON_DRIVER(TestEnvironment, driver_ops, "zircon", "0.1");
