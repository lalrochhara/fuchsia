// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#pragma once

#include <stdint.h>

#include <magenta/compiler.h>
#include <magenta/types.h>

__BEGIN_CDECLS;

typedef struct mxio_namespace mxio_ns_t;


// Create a new, empty namespace
mx_status_t mxio_ns_create(mxio_ns_t** out);

// Create a new directory within a namespace, bound to the
// directory-protocol-compatible handle h
// The path must be an absolute path, like "/x/y/z", containing
// no "." nor ".." entries.  It is relative to the root of the
// namespace.
//
// The handle is not closed on failure.
mx_status_t mxio_ns_bind(mxio_ns_t* ns, const char* path, mx_handle_t h);

// Create a new directory within a namespace, bound to the
// directory referenced by the file descriptor fd.
// The path must be an absolute path, like "/x/y/z", containing
// no "." nor ".." entries.  It is relative to the root of the
// namespace.
//
// The fd is not closed on success or failure.
// Closing the fd after success does not affect namespace.
mx_status_t mxio_ns_bind_fd(mxio_ns_t* ns, const char* path, int fd);

// Open the root directory of the namespace as a file descriptor
int mxio_ns_opendir(mxio_ns_t* ns);

// chdir to / in the provided namespace
mx_status_t mxio_ns_chdir(mxio_ns_t* ns);

// Replace the mxio "global" namespace with the provided namespace
mx_status_t mxio_ns_install(mxio_ns_t* ns);

__END_CDECLS;
