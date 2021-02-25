#!/bin/bash
# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

function usage() {
  cat <<EOF
usage: $0 DIR1 DIR2

Compares two build directories for artifact differences.
Comparison logic tries to account for the file type in
choosing a diff strategy.

Run this from the Fuchsia source checkout root dir ($FUCHSIA_DIR),
because it references some tools from the source tree.

Example: Compare two clean builds with same output dir:
  fx set ...
  fx clean-build
  cp -p -r out/default out/default.bkp
  fx clean-build
  $0 out/default.bkp out/default

EOF
}

# This JSON formatter only overwrites the file in place.
# Usage: json_format --format FILE
readonly json_format=third_party/catapult/telemetry/json_format

# GLOBAL MUTABLES
# Accumulate paths to unexpected differences here.
# bash: array variables are not POSIX
unexpected_diffs=()
unexpected_matches=()
# These files match or differ, but we didn't know what to expect.
unclassified_diffs=()
unclassified_matches=()

function diff_json() {
  # json_format doesn't have an option to output to stdout,
  # so we must copy it to temporary files.
  cp "$1"{,.formatted}
  cp "$2"{,.formatted}
  chmod +w "$1".formatted "$2.formatted"
  "$json_format" --format "$1".formatted || { echo "Failed to format $1" ; return 1;}
  "$json_format" --format "$2".formatted || { echo "Failed to format $2" ; return 1;}
  diff -u "$1".formatted "$2".formatted
  # return with the exit code of diff
}

function diff_text() {
  diff -u "$1" "$2"
}

function diff_binary() {
  diff -q "$1" "$2"
}

function diff_file_relpath() {
  # $1 is left dir
  # $2 is right dir
  # $3 is a relative path under both dirs, and is itself not a directory.
  #   This file name is reported in diagnostics.
  # one could also use an all-inclusive diff tool like https://diffoscope.org/
  local left="$1/$3"
  local right="$2/$3"
  local common_path="$3"
  # echo "Comparing file $3"
  filebase="$(basename "$common_path")"

  # TODO(fangism): Some files are stored as blobs so content differences
  # appear as filename entry differences.  Skip these.  Perhaps silently?
  if test ! -f "$left"
  then printf "%s does not exist\n" "$left"
    return
  fi
  if test ! -f "$right"
  then echo "%s does not exist\n" "$right"
    return
  fi

  # Classify each category of files with expectations in each case below:
  #   expect={diff,match,unknown,ignore}; diff...
  # Leave blank for ignored files (not compared).
  # "unknown" means unclassified and could contain a mix of matches/diffs.
  # Goal:
  #   * Identify and classify known differences.
  #   * Gradually reduce sources of differences.
  #
  # TODO(fangism): different expectations in clean-vs-clean /
  # clean-vs-incremental modes.
  case "$filebase" in
    # The exit status of this case statement will be used to determine
    # whether or not the given file is an erroneous diff.
    #
    # Generally:
    #   diff_text for text files that are expected to match
    #   diff_binary for binaries or known large textual differences

    # depfiles
    *.d) expect=match; diff_text "$left" "$right" ;;

    # C++ object files (binary)
    *.o)
      case "$common_path" in
        efi_x64/obj/src/*.c.o)
          expect=unknown; diff_binary "$left" "$right" ;;
        *) expect=match; diff_binary "$left" "$right" ;;
      esac
      # TODO(fangism): compare objdumps for details
      ;;

    # Ignore .a differences until .o differences have been eliminated.
    # Eventually, use diff_binary.
    *.a) expect=ignore ;;

    # Rust libraries (binary)
    *.rlib) expect=match; diff_binary "$left" "$right" ;;

    # Generated code
    *.rs)
      case "$common_path" in
        gen/src/*/qmi-protocol.rs)
          expect=diff; diff_text "$left" "$right" ;;  # ordering diff
        *) expect=match; diff_text "$left" "$right" ;;
      esac
      ;;

    memory_metrics_registry.cb.h)
      expect=diff; diff_text "$left" "$right" ;;  # ordering diff


    # The following groups of files have known huge diffs,
    # so omit details from the general report, and diff_binary.
    meta.far) expect=diff; diff_binary "$left" "$right" ;;
    meta.far.merkle) expect=diff; diff_binary "$left" "$right" ;;
    contents) expect=diff; diff_binary "$left" "$right" ;;
    blobs.json) expect=diff; diff_binary "$left" "$right" ;;
    blobs.manifest) expect=diff; diff_binary "$left" "$right" ;;
    package_manifest.json) expect=diff; diff_binary "$left" "$right" ;;
    targets.json)
      case "$common_path" in
        gen/gopaths/*)
          expect=match; diff_binary "$left" "$right" ;;
        *) expect=diff; diff_binary "$left" "$right" ;;  # diffs: many hashes
      esac
      ;;

    timestamp.json) expect=diff; diff_json "$left" "$right" ;;  # diffs: sig, expires, version
    elf_sizes.json) expect=diff; diff_json "$left" "$right" ;;  # diffs: build_id
    filesystem_sizes.json) expect=diff; diff_json "$left" "$right" ;;  # diffs: value
    *.zbi.json) expect=unknown; diff_json "$left" "$right" ;;  # diffs: crc32, size

    # Diff formatted JSON for readability.
    *.json) expect=match; diff_json "$left" "$right" ;;
    *.json.formatted) expect=ignore ;;  # This is remant from an earlier diff.

    # Binary.
    *.blk) expect=unknown; diff_binary "$left" "$right" ;;

    # Ignore ninja logs, as they bear timestamps,
    # and are non-essential build artifacts.
    .ninja.log) expect=ignore ;;

    # Ignore filesystem access trace files.
    # They may contain nondeterministic paths to /proc/PID
    *_trace.txt) expect=ignore ;;

    # like exe.unstripped/*.map files
    # Many of these (but not all) reference mktemp paths.
    *.map) expect=unknown; diff_binary "$left" "$right" ;;

    # Ignore stamp files.
    *.stamp) expect=ignore ;;

    # Ignore temporary and backup files.
    *.tmp) expect=ignore ;;
    *.bak) expect=ignore ;;
    *.bkp) expect=ignore ;;

    kazoo-golden-test)
      expect=diff; diff_text "$left" "$right" ;;  # timestamp

    # All others.
    # Binary files diffs will still only be reported tersely.
    *)
      file_type="$(file "$left" | head -n 1 | cut -d: -f2-)"
      case "$file_type" in
        # Binary examples:
        # ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV),... dynamically linked, interpreter ld.so.1, no section header
        # Mach-O universal binary with 2 architectures: [x86_64:Mach-O 64-bit executable x86_64] [arm64e:Mach-O 64-bit executable arm64e]
        *ELF*executable* | *Mach-O*binary*)
          expect=unknown; diff_binary "$left" "$right" ;;
        # Assume non-binaries are text.
        *) expect=match; diff_text "$left" "$right" ;;
      esac
  esac
  # Record unexpected differences.
  diff_status="$?"
  case "$expect" in
    match)
      test "$diff_status" = 0 || unexpected_diffs=("${unexpected_diffs[@]}" "$common_path") ;;
    diff)
      test "$diff_status" != 0 || unexpected_matches=("${unexpected_matches[@]}" "$common_path") ;;
    unknown)
      if test "$diff_status" = 0
      then unclassified_matches=("${unclassified_matches[@]}" "$common_path")
      else unclassified_diffs=("${unclassified_diffs[@]}" "$common_path")
      fi
      ;;
    *) ;; # ignore
  esac
}

function diff_dir_recursive() {
  # $1 and $2 are two directories, e.g. "out/default"
  # $3 is the relative path down from $1 and $2, e.g. "subdir/" or "".
  # For dual-traversal, arbitrarily use $2's subdirs.
  # echo "Comparing: $sub"
  local sub="$3"  # sub-dir or file

  # Ignore some dirs.
  case "$sub" in
    # Ignore files whose names are content-hash like.
    amber-files/repository/blobs/) return ;;
    amber-files/repository/targets/) return ;;
    *) ;;  # continue
  esac

  # Silence empty dirs.
  if ! ls "$2/$sub"*
  then return  # empty dir
  fi > /dev/null 2>&1

  for f in "$2/$sub"*
  do
    filebase="$(basename "$f")"
    relpath="$sub$filebase"
    if test -d "$f"
    then diff_dir_recursive "$1" "$2" "$relpath/"
      # TODO(fangism): what about test -L for symlinks?
    else diff_file_relpath "$1" "$2" "$relpath"
    fi
  done
}

test "$#" = 2 || { usage; exit 2; }

diff_dir_recursive "$1" "$2" ""

# Summarize findings:
exit_status=0

if test "${#unexpected_diffs[@]}" != 0
then
  echo "UNEXPECTED DIFFS: (action: fix source of difference)"
  for path in "${unexpected_diffs[@]}"
  do echo "  $path"
  done
  exit_status=1
fi

if test "${#unexpected_matches[@]}" != 0
then
  echo "UNEXPECTED MATCHES: (action: make these expect=match now?)"
  for path in "${unexpected_matches[@]}"
  do echo "  $path"
  done
  exit_status=1
fi

if test "${#unclassified_diffs[@]}" != 0
then
  echo "unclassified diffs: (action: classify them as expect=diff)"
  for path in "${unclassified_diffs[@]}"
  do echo "  $path"
  done
  # Leave exit status as it were.
fi

if test "${#unclassified_matches[@]}" != 0
then
  echo "unclassified matches: (action: classify them as expect=match)"
  for path in "${unclassified_matches[@]}"
  do echo "  $path"
  done
  # Leave exit status as it were.
fi

exit "$exit_status"
