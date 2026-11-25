# SPDX-License-Identifier: LGPL-3.0-or-later
require "digest"
require "English"

class Megu < Formula
  desc "A Minecraft-compatible engine written in Rust"
  homepage "https://github.com/nekomario28/megu-mc"
  version "0.1.0-alpha.3"
  license "LGPL-3.0-or-later"

  # The release publisher creates the checksum manifest from the exact archive
  # bytes. Fetch it at install time because prerelease assets are not checked
  # into this formula and the manifest itself is the integrity authority.
  on_macos do
    on_arm do
      url "https://github.com/nekomario28/megu-mc/releases/download/v#{version}/megu-v#{version}-macos-arm64.tar.gz"
    end
    on_intel do
      url "https://github.com/nekomario28/megu-mc/releases/download/v#{version}/megu-v#{version}-macos-x86_64.tar.gz"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/nekomario28/megu-mc/releases/download/v#{version}/megu-v#{version}-linux-arm64.tar.gz"
    end
    on_intel do
      url "https://github.com/nekomario28/megu-mc/releases/download/v#{version}/megu-v#{version}-linux-x86_64.tar.gz"
    end
  end

  def install
    sums_url = "https://github.com/nekomario28/megu-mc/releases/download/v#{version}/SHA256SUMS-all.txt"
    target = if OS.mac?
      Hardware::CPU.arm? ? "macos-arm64" : "macos-x86_64"
    else
      Hardware::CPU.arm? ? "linux-arm64" : "linux-x86_64"
    end
    archive = "megu-v#{version}-#{target}.tar.gz"
    sums_path = buildpath/"SHA256SUMS-all.txt"
    system "curl", "--fail", "--location", "--silent", "--show-error", "-o", sums_path, sums_url
    raise "Could not download release checksums" unless $CHILD_STATUS.success?
    sums = sums_path.read
    expected = sums.lines.filter_map do |line|
      match = line.match(/\A([0-9a-f]{64})  #{Regexp.escape(archive)}\n?\z/)
      match[1] if match
    end
    raise "Checksum entry missing or duplicated for #{archive}" unless expected.length == 1
    archive_path = cached_download
    raise "SHA-256 verification failed" unless Digest::SHA256.file(archive_path).hexdigest == expected.first
    bundle = "megu-v#{version}-#{target}"
    %w[megu megu-client-gpu megu-installer].each do |binary|
      bin.install "#{bundle}/#{binary}"
    end
  end

  test do
    assert_predicate bin/"megu", :executable?
    assert_match "megu", shell_output("#{bin}/megu --help", 0)
  end
end
