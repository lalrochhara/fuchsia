// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_TEST_UTIL_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_TEST_UTIL_H_

#include <lib/zx/event.h>
#include <zircon/device/network.h>

#include <memory>
#include <vector>

#include <fbl/intrusive_double_list.h>
#include <gtest/gtest.h>

#include "definitions.h"
#include "device_interface.h"

namespace network {
namespace testing {

constexpr uint16_t kRxDepth = 16;
constexpr uint16_t kTxDepth = 16;
constexpr uint16_t kDefaultDescriptorCount = 256;
constexpr uint64_t kDefaultBufferLength = ZX_PAGE_SIZE / 2;
constexpr uint32_t kAutoReturnRxLength = 512;

class RxReturnTransaction;
class TxReturnTransaction;
using VmoProvider = fit::function<zx::unowned_vmo(uint8_t)>;

class TxBuffer : public fbl::DoublyLinkedListable<std::unique_ptr<TxBuffer>> {
 public:
  explicit TxBuffer(const tx_buffer_t& buffer) : buffer_(buffer) {
    for (size_t i = 0; i < buffer_.data_count; i++) {
      parts_[i] = buffer_.data_list[i];
    }
    buffer_.data_list = parts_.data();
  }

  zx_status_t status() const { return status_; }

  void set_status(zx_status_t status) { status_ = status; }

  zx::status<std::vector<uint8_t>> GetData(const VmoProvider& vmo_provider);

  tx_result_t result() {
    return {
        .id = buffer_.id,
        .status = status_,
    };
  }

  tx_buffer_t& buffer() { return buffer_; }

 private:
  tx_buffer_t buffer_{};
  internal::BufferParts<buffer_region_t> parts_{};
  zx_status_t status_ = ZX_OK;
};

class RxBuffer : public fbl::DoublyLinkedListable<std::unique_ptr<RxBuffer>> {
 public:
  explicit RxBuffer(const rx_space_buffer_t& space)
      : space_(space),
        return_part_(rx_buffer_part_t{
            .id = space.id,
        }) {}

  zx_status_t WriteData(const std::vector<uint8_t>& data, const VmoProvider& vmo_provider) {
    return WriteData(fbl::Span(data.data(), data.size()), vmo_provider);
  }

  zx_status_t WriteData(fbl::Span<const uint8_t> data, const VmoProvider& vmo_provider);

  rx_buffer_part_t& return_part() { return return_part_; }
  rx_space_buffer_t& space() { return space_; }

  void SetReturnLength(uint32_t length) { return_part_.length = length; }

 private:
  rx_space_buffer_t space_{};
  rx_buffer_part_t return_part_{};
};

class RxReturn : public fbl::DoublyLinkedListable<std::unique_ptr<RxReturn>> {
 public:
  RxReturn()
      : buffer_(rx_buffer_t{
            .meta =
                {
                    .info_type = static_cast<uint32_t>(netdev::wire::InfoType::kNoInfo),
                    .frame_type = static_cast<uint8_t>(netdev::wire::FrameType::kEthernet),
                },
            .data_list = parts_.begin(),
            .data_count = 0,
        }) {}
  // RxReturn can't be moved because it keeps pointers to the return buffer internally.
  RxReturn(RxReturn&&) = delete;
  explicit RxReturn(std::unique_ptr<RxBuffer> buffer) : RxReturn() { PushPart(std::move(buffer)); }

  // Pushes buffer space into the return buffer.
  //
  // NB: We don't really need the unique pointer here, we just copy the information we need. But
  // requiring the unique pointer to be passed enforces the buffer ownership semantics. Also
  // RxBuffers usually sit in the available queue as a pointer already.
  void PushPart(std::unique_ptr<RxBuffer> buffer) {
    ZX_ASSERT(buffer_.data_count < parts_.size());
    parts_[buffer_.data_count++] = buffer->return_part();
  }

  const rx_buffer_t& buffer() const { return buffer_; }
  rx_buffer_t& buffer() { return buffer_; }

 private:
  internal::BufferParts<rx_buffer_part_t> parts_{};
  rx_buffer_t buffer_{};
};

constexpr zx_signals_t kEventStart = ZX_USER_SIGNAL_0;
constexpr zx_signals_t kEventStop = ZX_USER_SIGNAL_1;
constexpr zx_signals_t kEventTx = ZX_USER_SIGNAL_2;
constexpr zx_signals_t kEventSessionStarted = ZX_USER_SIGNAL_3;
constexpr zx_signals_t kEventRxAvailable = ZX_USER_SIGNAL_4;
constexpr zx_signals_t kEventPortRemoved = ZX_USER_SIGNAL_5;
constexpr zx_signals_t kEventPortActiveChanged = ZX_USER_SIGNAL_6;

class FakeNetworkDeviceImpl;

class FakeNetworkPortImpl : public ddk::NetworkPortProtocol<FakeNetworkPortImpl> {
 public:
  FakeNetworkPortImpl();
  ~FakeNetworkPortImpl();

  void NetworkPortGetInfo(port_info_t* out_info);
  void NetworkPortGetStatus(port_status_t* out_status);
  void NetworkPortSetActive(bool active);
  void NetworkPortGetMac(mac_addr_protocol_t* out_mac_ifc);
  void NetworkPortRemoved();

  port_info_t& port_info() { return port_info_; }
  const port_status_t& status() const { return status_; }
  void AddPort(uint8_t port_id, ddk::NetworkDeviceIfcProtocolClient ifc_client);
  void RemoveSync();
  void SetMac(mac_addr_protocol_t proto) { mac_proto_ = proto; }

  network_port_protocol_t protocol() {
    return {
        .ops = &network_port_protocol_ops_,
        .ctx = this,
    };
  }

  bool active() const { return port_active_; }
  bool removed() const { return port_removed_; }
  uint8_t id() const { return id_; }

  const zx::event& events() const { return event_; }

  void SetOnline(bool online);
  void SetStatus(const port_status_t& status);

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(FakeNetworkPortImpl);

  std::array<uint8_t, netdev::wire::kMaxFrameTypes> rx_types_;
  std::array<tx_support_t, netdev::wire::kMaxFrameTypes> tx_types_;
  ddk::NetworkDeviceIfcProtocolClient device_client_;
  fit::callback<void()> on_removed_;
  uint8_t id_;
  mac_addr_protocol_t mac_proto_{};
  port_info_t port_info_{};
  std::atomic_bool port_active_ = false;
  port_status_t status_{};
  zx::event event_;
  bool port_removed_ = false;
  bool port_added_ = false;
};

class FakeNetworkDeviceImpl : public ddk::NetworkDeviceImplProtocol<FakeNetworkDeviceImpl> {
 public:
  FakeNetworkDeviceImpl();
  ~FakeNetworkDeviceImpl();

  zx::status<std::unique_ptr<NetworkDeviceInterface>> CreateChild(async_dispatcher_t* dispatcher);

  zx_status_t NetworkDeviceImplInit(const network_device_ifc_protocol_t* iface);
  void NetworkDeviceImplStart(network_device_impl_start_callback callback, void* cookie);
  void NetworkDeviceImplStop(network_device_impl_stop_callback callback, void* cookie);
  void NetworkDeviceImplGetInfo(device_info_t* out_info);
  void NetworkDeviceImplQueueTx(const tx_buffer_t* buf_list, size_t buf_count);
  void NetworkDeviceImplQueueRxSpace(const rx_space_buffer_t* buf_list, size_t buf_count);
  void NetworkDeviceImplPrepareVmo(uint8_t vmo_id, zx::vmo vmo) {
    zx::vmo& slot = vmos_[vmo_id];
    EXPECT_FALSE(slot.is_valid()) << "vmo " << static_cast<uint32_t>(vmo_id) << " already prepared";
    slot = std::move(vmo);
  }
  void NetworkDeviceImplReleaseVmo(uint8_t vmo_id) {
    zx::vmo& slot = vmos_[vmo_id];
    EXPECT_TRUE(slot.is_valid()) << "vmo " << static_cast<uint32_t>(vmo_id) << " already released";
    slot.reset();
  }
  void NetworkDeviceImplSetSnoop(bool snoop) { /* do nothing , only auto-snooping is allowed */
  }

  fit::function<zx::unowned_vmo(uint8_t)> VmoGetter();

  const zx::event& events() const { return event_; }

  device_info_t& info() { return info_; }

  std::unique_ptr<RxBuffer> PopRxBuffer() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return rx_buffers_.pop_front();
  }

  std::unique_ptr<TxBuffer> PopTxBuffer() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return tx_buffers_.pop_front();
  }

  fbl::DoublyLinkedList<std::unique_ptr<TxBuffer>> TakeTxBuffers() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    fbl::DoublyLinkedList<std::unique_ptr<TxBuffer>> r;
    tx_buffers_.swap(r);
    return r;
  }

  fbl::DoublyLinkedList<std::unique_ptr<RxBuffer>> TakeRxBuffers() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    fbl::DoublyLinkedList<std::unique_ptr<RxBuffer>> r;
    rx_buffers_.swap(r);
    return r;
  }

  size_t rx_buffer_count() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return rx_buffers_.size_slow();
  }

  size_t tx_buffer_count() __TA_EXCLUDES(lock_) {
    fbl::AutoLock lock(&lock_);
    return tx_buffers_.size_slow();
  }

  std::optional<uint8_t> first_vmo_id() {
    for (size_t i = 0; i < vmos_.size(); i++) {
      if (vmos_[i].is_valid()) {
        return i;
      }
    }
    return std::nullopt;
  }

  void set_auto_start(bool auto_start) { auto_start_ = auto_start; }

  void set_auto_stop(bool auto_stop) { auto_stop_ = auto_stop; }

  bool TriggerStart();
  bool TriggerStop();

  network_device_impl_protocol_t proto() {
    return network_device_impl_protocol_t{.ops = &network_device_impl_protocol_ops_, .ctx = this};
  }

  void set_immediate_return_tx(bool auto_return) { immediate_return_tx_ = auto_return; }
  void set_immediate_return_rx(bool auto_return) { immediate_return_rx_ = auto_return; }

  ddk::NetworkDeviceIfcProtocolClient& client() { return device_client_; }

  fbl::Span<const zx::vmo> vmos() { return fbl::Span(vmos_.begin(), vmos_.end()); }

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(FakeNetworkDeviceImpl);

  fbl::Mutex lock_;
  std::array<zx::vmo, MAX_VMOS> vmos_;
  device_info_t info_{};
  ddk::NetworkDeviceIfcProtocolClient device_client_;
  fbl::DoublyLinkedList<std::unique_ptr<RxBuffer>> rx_buffers_ __TA_GUARDED(lock_);
  fbl::DoublyLinkedList<std::unique_ptr<TxBuffer>> tx_buffers_ __TA_GUARDED(lock_);
  zx::event event_;
  bool auto_start_ = true;
  bool auto_stop_ = true;
  bool immediate_return_tx_ = false;
  bool immediate_return_rx_ = false;
  bool device_started_ __TA_GUARDED(lock_) = false;
  fit::function<void()> pending_start_callback_ __TA_GUARDED(lock_);
  fit::function<void()> pending_stop_callback_ __TA_GUARDED(lock_);
};

class TestSession {
 public:
  static constexpr uint16_t kDefaultDescriptorCount = 256;
  static constexpr uint64_t kDefaultBufferLength = ZX_PAGE_SIZE / 2;

  TestSession() = default;

  zx_status_t Open(fidl::WireSyncClient<netdev::Device>& netdevice, const char* name,
                   netdev::wire::SessionFlags flags = netdev::wire::SessionFlags::kPrimary,
                   uint16_t num_descriptors = kDefaultDescriptorCount,
                   uint64_t buffer_size = kDefaultBufferLength);

  zx_status_t Init(uint16_t descriptor_count, uint64_t buffer_size);
  zx::status<netdev::wire::SessionInfo> GetInfo();
  void Setup(fidl::ClientEnd<netdev::Session> session, netdev::wire::Fifos fifos);
  [[nodiscard]] zx_status_t AttachPort(uint8_t port_id,
                                       std::vector<netdev::wire::FrameType> frame_types);
  [[nodiscard]] zx_status_t AttachPort(FakeNetworkPortImpl& impl);
  [[nodiscard]] zx_status_t DetachPort(uint8_t port_id);
  [[nodiscard]] zx_status_t DetachPort(FakeNetworkPortImpl& impl);

  zx_status_t Close();
  zx_status_t WaitClosed(zx::time deadline);
  void ZeroVmo();
  buffer_descriptor_t* ResetDescriptor(uint16_t index);
  buffer_descriptor_t* descriptor(uint16_t index);
  uint8_t* buffer(uint64_t offset);

  zx_status_t FetchRx(uint16_t* descriptors, size_t count, size_t* actual) const;
  zx_status_t FetchTx(uint16_t* descriptors, size_t count, size_t* actual) const;
  zx_status_t SendRx(const uint16_t* descriptor, size_t count, size_t* actual) const;
  zx_status_t SendTx(const uint16_t* descriptor, size_t count, size_t* actual) const;
  zx_status_t SendTxData(uint16_t descriptor_index, const std::vector<uint8_t>& data);

  zx_status_t FetchRx(uint16_t* descriptor) const {
    size_t actual;
    return FetchRx(descriptor, 1, &actual);
  }

  zx_status_t FetchTx(uint16_t* descriptor) const {
    size_t actual;
    return FetchTx(descriptor, 1, &actual);
  }

  zx_status_t SendRx(uint16_t descriptor) const {
    size_t actual;
    return SendRx(&descriptor, 1, &actual);
  }

  zx_status_t SendTx(uint16_t descriptor) const {
    size_t actual;
    return SendTx(&descriptor, 1, &actual);
  }

  fidl::WireSyncClient<netdev::Session>& session() { return session_; }

  uint64_t canonical_offset(uint16_t index) const { return buffer_length_ * index; }

  const zx::fifo& tx_fifo() const { return fifos_.tx; }
  const zx::channel& channel() const { return session_.channel(); }

 private:
  fidl::FidlAllocator<> alloc_;
  uint16_t descriptors_count_{};
  uint64_t buffer_length_{};
  fidl::WireSyncClient<netdev::Session> session_;
  zx::vmo data_vmo_;
  fzl::VmoMapper data_;
  zx::vmo descriptors_vmo_;
  fzl::VmoMapper descriptors_;
  netdev::wire::Fifos fifos_;
};

class RxReturnTransaction {
 public:
  explicit RxReturnTransaction(FakeNetworkDeviceImpl* impl) : client_(impl->client()) {}

  void Enqueue(std::unique_ptr<RxReturn> buffer) {
    return_buffers_[count_++] = buffer->buffer();
    buffers_.push_back(std::move(buffer));
  }

  void Enqueue(std::unique_ptr<RxBuffer> buffer) {
    Enqueue(std::make_unique<RxReturn>(std::move(buffer)));
  }

  void Commit() {
    client_.CompleteRx(return_buffers_, count_);
    count_ = 0;
    buffers_.clear();
  }

 private:
  rx_buffer_t return_buffers_[kRxDepth]{};
  size_t count_ = 0;
  ddk::NetworkDeviceIfcProtocolClient client_;
  fbl::DoublyLinkedList<std::unique_ptr<RxReturn>> buffers_;

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(RxReturnTransaction);
};

class TxReturnTransaction {
 public:
  explicit TxReturnTransaction(FakeNetworkDeviceImpl* impl) : client_(impl->client()) {}

  void Enqueue(std::unique_ptr<TxBuffer> buffer) { return_buffers_[count_++] = buffer->result(); }

  void Commit() {
    client_.CompleteTx(return_buffers_, count_);
    count_ = 0;
  }

 private:
  tx_result_t return_buffers_[kRxDepth]{};
  size_t count_ = 0;
  ddk::NetworkDeviceIfcProtocolClient client_;

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(TxReturnTransaction);
};

}  // namespace testing
}  // namespace network

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_TEST_UTIL_H_
