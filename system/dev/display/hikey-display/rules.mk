# Copyright 2017 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

LOCAL_DIR := $(GET_LOCAL_DIR)

MODULE := $(LOCAL_DIR)

MODULE_TYPE := driver

MODULE_SRCS += \
    $(LOCAL_DIR)/dsi.c \
    $(LOCAL_DIR)/adv7533.c \
    $(LOCAL_DIR)/edid.c

MODULE_STATIC_LIBS := system/ulib/ddk system/ulib/sync system/ulib/pretty

MODULE_LIBS := system/ulib/driver system/ulib/zircon system/ulib/c

MODULE_BANJO_LIBS := \
    system/banjo/ddk-protocol-gpio

include make/module.mk
