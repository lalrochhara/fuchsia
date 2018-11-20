// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#pragma once

#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <lib/async/dispatcher.h>
#include <lib/memfs/cpp/vnode.h>
#include <lib/zx/vmo.h>

namespace bootsvc {

class BootfsService : public fbl::RefCounted<BootfsService> {
public:
    ~BootfsService();

    // Create a BootfsService from the given bootfs blob, and set up
    // its VFS to use the given async dispatcher.
    static zx_status_t Create(zx::vmo bootfs_vmo, async_dispatcher_t* dispatcher,
                              fbl::RefPtr<BootfsService>* out);

    // Creates a connection to the root of the bootfs VFS and returns
    // a channel that can be used to speak the fuchsia.io.Node interface.
    zx_status_t CreateRootConnection(zx::channel* out);

    // Looks up the given path in the bootfs and returns its contents and size.
    zx_status_t Open(const char* path, zx::vmo* vmo, size_t* size);

    // Publishes the given |vmo| range into the bootfs at |path|.  |path| should
    // not begin with a slash and be relative to the root of the bootfs.  |vmo|
    // may not be closed until after BootfsService is destroyed.
    zx_status_t PublishVmo(const char* path, const zx::vmo& vmo, zx_off_t off, size_t len);

    // Publishes all of the VMOs from the startup handles table with the given
    // |type|.  |debug_type_name| is used for debug printing.
    void PublishStartupVmos(uint8_t type, const char* debug_type_name);
private:
    BootfsService() = default;

    zx::vmo bootfs_;

    memfs::Vfs vfs_;
    // root of the vfs
    fbl::RefPtr<memfs::VnodeDir> root_;
};

} // namespace bootsvc
