// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/result.h>
#include <lib/ftl/ndm-driver.h>
#include <lib/ftl/volume.h>

#include <algorithm>
#include <cstdint>
#include <memory>
#include <string>
#include <utility>

#include <fbl/algorithm.h>
#include <fbl/ref_ptr.h>
#include <fbl/span.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/storage/volume_image/address_descriptor.h"
#include "src/storage/volume_image/ftl/ftl_image.h"
#include "src/storage/volume_image/ftl/ftl_raw_nand_image_writer.h"
#include "src/storage/volume_image/ftl/options.h"
#include "src/storage/volume_image/ftl/raw_nand_image.h"
#include "src/storage/volume_image/ftl/raw_nand_image_utils.h"
#include "src/storage/volume_image/partition.h"
#include "src/storage/volume_image/utils/block_utils.h"
#include "src/storage/volume_image/utils/reader.h"
#include "src/storage/volume_image/utils/writer.h"
#include "src/storage/volume_image/volume_descriptor.h"

namespace storage::volume_image {
namespace {

constexpr uint64_t kBlockSize = 4096;
static_assert(kBlockSize % 4 == 0, "Blocks must be 32-bit aligned to simplify content generation.");

constexpr uint64_t kPageSize = 8192;
constexpr uint32_t kOobBytesSize = 16;
constexpr uint64_t kPagesPerBlock = 32;
constexpr uint64_t kBlockCount = 20;

RawNandOptions GetOptions() {
  RawNandOptions options;
  options.oob_bytes_size = kOobBytesSize;
  options.page_size = kPageSize;
  options.pages_per_block = kPagesPerBlock;
  options.page_count = kPagesPerBlock * kBlockCount;
  return options;
}

// This test provides proof of concept and verifies that an image generated by the the FtlImageWrite
// interface, allows the FTL driver to bootstrap.
//
//
// The first test, checks that the default image, without adjusting page size will be loaded.
// TODO(gevalentino): Once the respective logic is added, the following tests must be added.
//
//     * The third test, verifies that the FVM in the FTL image is bootstrapped
//     appropriately, by the FTL driver.
//
//  This is tracked as part of the  FTL image generation stack.

void FillBlock(uint32_t block_number, size_t block_offset, fbl::Span<uint8_t> block_view) {
  uint8_t* content = reinterpret_cast<uint8_t*>(&block_number);

  for (size_t i = 0; i < block_view.size(); ++i) {
    block_view[i] = content[(block_offset + i) % 4];
  }
}

// This reader provides the contents to be written into the image.
// Each block consists of repeated 32 bit integers containing the block number.
// Each block is of |kBlockSize|.
class FakeContentReader final : public Reader {
 public:
  uint64_t GetMaximumOffset() const override { return 0; }

  fit::result<void, std::string> Read(uint64_t offset, fbl::Span<uint8_t> buffer) const final {
    // Calculate the block the offset is in.
    uint32_t first_block = GetBlockFromBytes(offset, kBlockSize);
    uint64_t offset_from_first_block = GetOffsetFromBlockStart(offset, kBlockSize);
    uint64_t read_bytes = 0;
    auto first_block_view = buffer.subspan(0, kBlockSize - offset_from_first_block);
    FillBlock(first_block, offset_from_first_block, first_block_view);
    read_bytes += first_block_view.size();

    // Remaining blocks are aligned.
    uint64_t block_count = GetBlockCount(offset, buffer.size(), kBlockSize);

    for (uint64_t current_block = first_block + 1; current_block < first_block + block_count;
         ++current_block) {
      size_t length = std::min(kBlockSize, buffer.size() - read_bytes);
      auto block_view = buffer.subspan(read_bytes, length);
      FillBlock(current_block, 0, block_view);
      read_bytes += block_view.size();
    }

    return fit::ok();
  }
};

struct InMemoryRawNand {
  RawNandOptions options;
  std::map<uint32_t, std::vector<uint8_t>> page_data;
  std::map<uint32_t, std::vector<uint8_t>> page_oob;
};

class InMemoryWriter final : public Writer {
 public:
  explicit InMemoryWriter(InMemoryRawNand* raw_nand) : raw_nand_(raw_nand) {}

  fit::result<void, std::string> Write(uint64_t offset, fbl::Span<const uint8_t> buffer) final {
    // Calculate page number based on adjusted offset.
    uint64_t adjusted_page_size = RawNandImageGetAdjustedPageSize(raw_nand_->options);
    uint64_t page_number = offset / adjusted_page_size;
    // Check if its OOB or page data based on the offset.
    if (offset % adjusted_page_size == 0) {
      auto page_view = buffer.subspan(0, raw_nand_->options.page_size);
      raw_nand_->page_data[page_number] = std::vector<uint8_t>(page_view.begin(), page_view.end());
    } else if (offset % adjusted_page_size == raw_nand_->options.page_size) {
      auto oob_view = buffer.subspan(0, raw_nand_->options.oob_bytes_size);
      raw_nand_->page_oob[page_number] = std::vector<uint8_t>(oob_view.begin(), oob_view.end());
    } else {
      return fit::error("Invalid Offset.");
    }

    return fit::ok();
  }

 private:
  InMemoryRawNand* raw_nand_ = nullptr;
};

class Ndm final : public ftl::NdmBaseDriver {
 public:
  explicit Ndm(InMemoryRawNand* raw_nand) : raw_nand_(raw_nand) {}

  // Performs driver initialization. Returns an error string, or nullptr on
  // success.
  const char* Init() final { return nullptr; }

  // Creates a new volume. Note that multiple volumes are not supported.
  // |ftl_volume| (if provided) will be notified with the volume details.
  // Returns an error string, or nullptr on success.
  const char* Attach(const ftl::Volume* ftl_volume) final {
    ftl::VolumeOptions options;
    options.block_size = raw_nand_->options.page_size * raw_nand_->options.pages_per_block;
    options.eb_size = raw_nand_->options.oob_bytes_size;
    options.max_bad_blocks = 0;
    options.num_blocks = raw_nand_->options.page_count / raw_nand_->options.pages_per_block + 1;
    options.page_size = raw_nand_->options.page_size;
    options.flags = 0;
    return CreateNdmVolume(ftl_volume, options);
  }

  // Destroy the volume created with Attach(). Returns true on success.
  bool Detach() final { return true; }

  // Reads |page_count| pages starting at |start_page|, placing the results on
  // |page_buffer| and |oob_buffer|. Either pointer can be nullptr if that
  // part is not desired.
  // Returns kNdmOk, kNdmUncorrectableEcc, kNdmFatalError or kNdmUnsafeEcc.
  int NandRead(uint32_t start_page, uint32_t page_count, void* page_buffer,
               void* oob_buffer) final {
    for (uint32_t i = 0; i < page_count; ++i) {
      uint32_t page_number = start_page + i;
      size_t page_offset = i * kPageSize;
      size_t oob_offset = i * kOobBytesSize;

      if (raw_nand_->page_data.find(page_number) == raw_nand_->page_data.end()) {
        if (page_buffer != nullptr) {
          auto page_view =
              fbl::Span<uint8_t>(reinterpret_cast<uint8_t*>(page_buffer) + page_offset, kPageSize);
          std::fill(page_view.begin(), page_view.end(), 0xFF);
        }
        if (oob_buffer != nullptr) {
          auto oob_view = fbl::Span<uint8_t>(reinterpret_cast<uint8_t*>(oob_buffer) + oob_offset,
                                             kOobBytesSize);
          std::fill(oob_view.begin(), oob_view.end(), 0xFF);
        }
      } else {
        if (page_buffer != nullptr) {
          auto page_view =
              fbl::Span<uint8_t>(reinterpret_cast<uint8_t*>(page_buffer) + page_offset, kPageSize);
          memcpy(page_view.data(), raw_nand_->page_data.at(page_number).data(), page_view.size());
        }

        if (oob_buffer != nullptr) {
          auto oob_view = fbl::Span<uint8_t>(reinterpret_cast<uint8_t*>(oob_buffer) + oob_offset,
                                             kOobBytesSize);
          memcpy(oob_view.data(), raw_nand_->page_oob.at(page_number).data(), oob_view.size());
        }
      }
    }
    return ftl::kNdmOk;
  }

  // Writes |page_count| pages starting at |start_page|, using the data from
  // |page_buffer| and |oob_buffer|.
  // Returns kNdmOk, kNdmError or kNdmFatalError. kNdmError triggers marking
  // the block as bad.
  int NandWrite(uint32_t start_page, uint32_t page_count, const void* page_buffer,
                const void* oob_buffer) final {
    for (uint32_t i = 0; i < page_count; ++i) {
      uint32_t page_number = start_page + i;
      size_t page_offset = i * kPageSize;
      size_t oob_offset = i * kOobBytesSize;
      auto page_view =
          fbl::Span<const uint8_t>(reinterpret_cast<const uint8_t*>(page_buffer) + page_offset,
                                   raw_nand_->options.page_size);
      auto oob_view =
          fbl::Span<const uint8_t>(reinterpret_cast<const uint8_t*>(oob_buffer) + oob_offset,
                                   raw_nand_->options.oob_bytes_size);

      if (page_buffer != nullptr) {
        raw_nand_->page_data[page_number] =
            std::vector<uint8_t>(page_view.begin(), page_view.end());
      }

      if (oob_buffer != nullptr) {
        raw_nand_->page_oob[page_number] = std::vector<uint8_t>(oob_view.begin(), oob_view.end());
      }
      return ftl::kNdmOk;
    }

    return ftl::kNdmOk;
  }

  // Erases the block containing |page_num|.
  // Returns kNdmOk or kNdmError. kNdmError triggers marking the block as bad.
  int NandErase(uint32_t page_num) final {
    uint32_t page_start = fbl::round_down(page_num, raw_nand_->options.pages_per_block);
    for (size_t i = 0; i < raw_nand_->options.pages_per_block; ++i) {
      raw_nand_->page_data.erase(page_start + i);
      raw_nand_->page_oob.erase(page_start + i);
    }
    return ftl::kNdmOk;
  }

  // Returns whether the block containing |page_num| was factory-marked as bad.
  // Returns kTrue, kFalse or kNdmError.
  int IsBadBlock(uint32_t page_num) final { return ftl::kFalse; }

  // Returns whether a given page is empty or not. |data| and |spare| store
  // the contents of the page.
  bool IsEmptyPage(uint32_t page_num, const uint8_t* data, const uint8_t* spare) final {
    auto page_view = fbl::Span<const uint8_t>(reinterpret_cast<const uint8_t*>(data), kPageSize);
    auto oob_view =
        fbl::Span<const uint8_t>(reinterpret_cast<const uint8_t*>(spare), kOobBytesSize);

    return std::all_of(oob_view.begin(), oob_view.end(), [](const auto& b) { return b == 0xFF; }) &&
           std::all_of(page_view.begin(), page_view.end(), [](const auto& b) { return b == 0xFF; });
  }

 private:
  InMemoryRawNand* raw_nand_ = nullptr;
};

class FakeFtl final : public ftl::FtlInstance {
 public:
  bool OnVolumeAdded(uint32_t page_size, uint32_t num_pages) final { return true; }
};

Partition MakePartition() {
  VolumeDescriptor volume_descriptor;
  volume_descriptor.name = "Hello Partition";
  volume_descriptor.block_size = 8192;

  AddressDescriptor address_descriptor;
  address_descriptor.mappings = {
      {.source = 512,
       .target = 8192,
       .count = 4096,
       .size = 4096,
       .options = {std::make_pair(EnumAsString(AddressMapOption::kFill), 0)}},
      {.source = 10002,
       .target = 0,
       .count = 0,
       .size = 8192,
       .options = {std::make_pair(EnumAsString(AddressMapOption::kFill), 0)}},
      {.source = 20000, .target = 81920, .count = 81920},
  };

  return Partition(std::move(volume_descriptor), std::move(address_descriptor),
                   std::make_unique<FakeContentReader>());
}

class FtlEnvironment : public ::testing::Environment {
  void SetUp() override { ftl::InitModules(); }
};

[[maybe_unused]] auto* environment = testing::AddGlobalTestEnvironment(new FtlEnvironment());

TEST(FtlImageBootstrapTest, FtlDriverBootstrapsFromImageIsOk) {
  [[maybe_unused]] auto partition = MakePartition();
  std::unique_ptr<InMemoryRawNand> raw_nand = std::make_unique<InMemoryRawNand>();
  raw_nand->options = GetOptions();

  InMemoryWriter writer(raw_nand.get());
  auto image_write_result = FtlImageWrite(raw_nand->options, partition, &writer);

  std::unique_ptr<Ndm> ndm_driver = std::make_unique<Ndm>(raw_nand.get());
  FakeFtl fake_ftl;
  ftl::VolumeImpl ftl_volume(&fake_ftl);
  const char* result = ftl_volume.Init(std::move(ndm_driver));
  ASSERT_EQ(result, nullptr) << result;

  // First mapping.
  std::vector<uint8_t> page_buffer(raw_nand->options.page_size, 0xFF);
  ASSERT_EQ(ftl_volume.Read(1, 1, page_buffer.data()), ZX_OK);

  std::vector<uint8_t> expected_page_buffer(raw_nand->options.page_size, 0xFF);
  ASSERT_TRUE(partition.reader()->Read(512, expected_page_buffer).is_ok());

  EXPECT_THAT(fbl::Span<uint8_t>(page_buffer).subspan(0, 4096),
              testing::ElementsAreArray(fbl::Span<uint8_t>(expected_page_buffer).subspan(0, 4096)));
  // Remainder of a mapping fitting on the same page, is filled with zeroes.
  EXPECT_THAT(fbl::Span<uint8_t>(page_buffer).subspan(4096, 4096), testing::Each(testing::Eq(0)));

  // Second mapping.
  ASSERT_EQ(ftl_volume.Read(0, 1, page_buffer.data()), ZX_OK);
  EXPECT_THAT(fbl::Span<uint8_t>(page_buffer).subspan(0, 8192), testing::Each(testing::Eq(0)));

  // Third mapping.
  std::fill(expected_page_buffer.begin(), expected_page_buffer.end(), 0);
  std::fill(page_buffer.begin(), page_buffer.end(), 0xFF);
  expected_page_buffer.resize(81920, 0);
  page_buffer.resize(81920, 0xFF);
  ASSERT_TRUE(partition.reader()->Read(20000, expected_page_buffer).is_ok());

  ASSERT_EQ(ftl_volume.Read(10, 10, page_buffer.data()), ZX_OK);
  EXPECT_THAT(page_buffer, testing::ElementsAreArray(expected_page_buffer));
}

// Stitches pages in |raw_nand| into bigger pages, such that page | 2i | 2i + 1| is page content for
// |page i| and same applies for OOB bytes. In the example, |logical_pages_per_physical_pages| is 2.
std::unique_ptr<InMemoryRawNand> CombinePages(uint32_t logical_pages_per_physical_pages,
                                              std::unique_ptr<InMemoryRawNand> raw_nand) {
  std::unique_ptr<InMemoryRawNand> stitched_raw_nand = std::make_unique<InMemoryRawNand>();
  stitched_raw_nand->options = raw_nand->options;
  stitched_raw_nand->options.oob_bytes_size *= logical_pages_per_physical_pages;
  stitched_raw_nand->options.page_size *= logical_pages_per_physical_pages;
  stitched_raw_nand->options.pages_per_block /= logical_pages_per_physical_pages;
  stitched_raw_nand->options.page_count /= logical_pages_per_physical_pages;

  for (auto [key, _] : raw_nand->page_data) {
    uint32_t page_number = key / logical_pages_per_physical_pages;
    uint32_t page_relative_offset = key % logical_pages_per_physical_pages;

    const auto& original_data = raw_nand->page_data[key];
    const auto& original_oob = raw_nand->page_oob[key];

    if (stitched_raw_nand->page_data.find(page_number) == stitched_raw_nand->page_data.end()) {
      stitched_raw_nand->page_data[page_number].resize(stitched_raw_nand->options.page_size, 0xFF);
      stitched_raw_nand->page_oob[page_number].resize(stitched_raw_nand->options.oob_bytes_size,
                                                      0xFF);
    }

    auto& stitched_data = stitched_raw_nand->page_data[page_number];
    auto& stitched_oob = stitched_raw_nand->page_oob[page_number];

    memcpy(stitched_data.data() + page_relative_offset * raw_nand->options.page_size,
           original_data.data(), raw_nand->options.page_size);
    memcpy(stitched_oob.data() + page_relative_offset * raw_nand->options.oob_bytes_size,
           original_oob.data(), raw_nand->options.oob_bytes_size);
  }

  return stitched_raw_nand;
}

class InMemoryWriterWithHeader : public Writer {
 public:
  explicit InMemoryWriterWithHeader(InMemoryWriter* writer) : writer_(writer) {}

  fit::result<void, std::string> Write(uint64_t offset, fbl::Span<const uint8_t> buffer) final {
    if (offset < sizeof(RawNandImageHeader)) {
      uint32_t leading_header_bytes =
          std::min(static_cast<size_t>(sizeof(RawNandImageHeader) - offset), buffer.size());
      memcpy(reinterpret_cast<uint8_t*>(&header_) + offset, buffer.data(), leading_header_bytes);
      if (leading_header_bytes == buffer.size()) {
        return fit::ok();
      }
      buffer.subspan(leading_header_bytes);
      offset = sizeof(RawNandImageHeader);
    }

    return writer_->Write(offset - sizeof(RawNandImageHeader), buffer);
  }

  const auto& header() { return header_; }

 private:
  RawNandImageHeader header_;
  InMemoryWriter* writer_ = nullptr;
};

TEST(FtlImageBootstrapTest, FtlDriverBootstrapsFromImageWithPageDoubleIsOk) {
  [[maybe_unused]] auto partition = MakePartition();
  std::unique_ptr<InMemoryRawNand> raw_nand = std::make_unique<InMemoryRawNand>();
  auto options = GetOptions();
  options.oob_bytes_size /= 2;
  options.page_size /= 2;
  options.page_count *= 2;
  options.pages_per_block *= 2;
  raw_nand->options = options;

  InMemoryWriter data_writer(raw_nand.get());
  InMemoryWriterWithHeader writer(&data_writer);

  std::vector<RawNandImageFlag> flags = {RawNandImageFlag::kRequireWipeBeforeFlash};
  auto ftl_raw_nand_image_writer_result =
      FtlRawNandImageWriter::Create(options, flags, ImageFormat::kRawImage, &writer);
  ASSERT_TRUE(ftl_raw_nand_image_writer_result.is_ok()) << ftl_raw_nand_image_writer_result.error();
  auto [ftl_raw_nand_image_writer, ftl_options] = ftl_raw_nand_image_writer_result.take_value();

  auto image_write_result = FtlImageWrite(ftl_options, partition, &ftl_raw_nand_image_writer);
  ASSERT_TRUE(image_write_result.is_ok()) << image_write_result.error();

  auto stitched_raw_nand = CombinePages(2, std::move(raw_nand));

  std::unique_ptr<Ndm> ndm_driver = std::make_unique<Ndm>(stitched_raw_nand.get());
  FakeFtl fake_ftl;
  ftl::VolumeImpl ftl_volume(&fake_ftl);
  const char* result = ftl_volume.Init(std::move(ndm_driver));
  ASSERT_EQ(result, nullptr) << result;

  // First mapping.
  std::vector<uint8_t> page_buffer(stitched_raw_nand->options.page_size, 0xFF);
  ASSERT_EQ(ftl_volume.Read(1, 1, page_buffer.data()), ZX_OK);

  std::vector<uint8_t> expected_page_buffer(stitched_raw_nand->options.page_size, 0xFF);
  ASSERT_TRUE(partition.reader()->Read(512, expected_page_buffer).is_ok());

  EXPECT_THAT(fbl::Span<uint8_t>(page_buffer).subspan(0, 4096),
              testing::ElementsAreArray(fbl::Span<uint8_t>(expected_page_buffer).subspan(0, 4096)));
  // Remainder of a mapping fitting on the same page, is filled with zeroes.
  EXPECT_THAT(fbl::Span<uint8_t>(page_buffer).subspan(4096, 4096), testing::Each(testing::Eq(0)));

  // Second mapping.
  ASSERT_EQ(ftl_volume.Read(0, 1, page_buffer.data()), ZX_OK);
  EXPECT_THAT(fbl::Span<uint8_t>(page_buffer).subspan(0, 8192), testing::Each(testing::Eq(0)));

  // Third mapping.
  std::fill(expected_page_buffer.begin(), expected_page_buffer.end(), 0);
  std::fill(page_buffer.begin(), page_buffer.end(), 0xFF);
  expected_page_buffer.resize(81920, 0);
  page_buffer.resize(81920, 0xFF);
  ASSERT_TRUE(partition.reader()->Read(20000, expected_page_buffer).is_ok());

  ASSERT_EQ(ftl_volume.Read(10, 10, page_buffer.data()), ZX_OK);
  EXPECT_THAT(page_buffer, testing::ElementsAreArray(expected_page_buffer));
}

}  // namespace
}  // namespace storage::volume_image
