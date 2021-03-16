// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "bootstrap_fidl_impl.h"

#include <lib/async/cpp/task.h>
#include <lib/fidl/llcpp/server.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/status.h>

#include <src/lib/files/file.h>
#include <src/lib/fsl/vmo/strings.h>

#include "thread_config_manager.h"

namespace ot {
namespace Fuchsia {
namespace {
constexpr char kMigrationConfigPath[] = "/config/data/migration_config.json";
}  // namespace

// BootstrapThreadImpl definitions -------------------------------------------------------

BootstrapThreadImpl::BootstrapThreadImpl() {}

BootstrapThreadImpl::~BootstrapThreadImpl() {
  StopServingFidl();

  if (binding_) {
    // If server is getting destroyed when there is
    // still an active binding, close binding with epitaph
    // informing client that the server has closed down:
    CloseBinding(ZX_ERR_PEER_CLOSED);
  }
}

zx_status_t BootstrapThreadImpl::Bind(fidl::ServerEnd<fuchsia_lowpan_bootstrap::Thread> request,
                                      async_dispatcher_t* dispatcher,
                                      cpp17::optional<const fbl::RefPtr<fs::PseudoDir>> svc_dir) {
  if (!ShouldServe()) {
    return ZX_OK;
  }

  auto result = fidl::BindServer(dispatcher, std::move(request), this);
  if (!result.is_ok()) {
    return result.error();
  }
  binding_ = result.take_value();

  // Note the svc_dir_ with which AddEntry was done, so that RemoveEntry can
  // be done when we want to stop serving this FIDL:
  svc_dir_ = svc_dir;

  return ZX_OK;
}

void BootstrapThreadImpl::StopServingFidl() {
  if (svc_dir_) {
    FX_LOGS(INFO) << "Removing svc entry";
    svc_dir_.value()->RemoveEntry(fuchsia_lowpan_bootstrap::Thread::Name);
    svc_dir_.reset();
  }
}

void BootstrapThreadImpl::CloseBinding(zx_status_t close_binding_status) {
  if (binding_) {
    FX_LOGS(INFO) << "Closing server binding";
    binding_->Close(close_binding_status);
    binding_.reset();
  }
}

void BootstrapThreadImpl::CloseBinding(zx_status_t close_binding_status,
                                       ImportSettingsCompleter::Sync& completer) {
  if (binding_) {
    FX_LOGS(INFO) << "Closing server binding";
    completer.Close(close_binding_status);
    binding_.reset();
  }
}

void BootstrapThreadImpl::ImportSettings(fuchsia_mem::wire::Buffer thread_settings_json,
                                         ImportSettingsCompleter::Sync& completer) {
  std::string data;

  fsl::SizedVmo sized_vmo(std::move(thread_settings_json.vmo), thread_settings_json.size);

  if (!fsl::StringFromVmo(sized_vmo, &data)) {
    FX_LOGS(ERROR) << "Failed to get data from VMO.";
    StopServingFidl();
    CloseBinding(ZX_ERR_IO, completer);
    return;
  }

  if (!files::WriteFile(GetSettingsPath(), data.data(), data.size())) {
    FX_LOGS(ERROR) << "Failed to write data to internal config location";
    StopServingFidl();
    CloseBinding(ZX_ERR_IO, completer);
    return;
  }

  completer.Reply();
  FX_LOGS(INFO) << "Done with ImportSettings!";
  StopServingFidl();
  CloseBinding(ZX_OK);
}

bool BootstrapThreadImpl::ShouldServe() { return files::IsFile(kMigrationConfigPath); }

std::string BootstrapThreadImpl::GetSettingsPath() { return kThreadSettingsPath; }

}  // namespace Fuchsia
}  // namespace ot
