class Zerostack < Formula
  desc "Minimalistic coding agent written in Rust, optimized for memory footprint and performance"
  homepage "https://github.com/sebahrens/mini-agent"
  version "1.10.4"
  license "GPL-3.0-only"

  on_macos do
    if Hardware::CPU.intel?
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.10.4/mini-agent-x86_64-apple-darwin.tar.gz"
      sha256 "61da48828eb89626e45b3092a79f559eb87a6df444cc2b98ad1dd68a205b5d95"
    else
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.10.4/mini-agent-aarch64-apple-darwin.tar.gz"
      sha256 "6e40fdb121715844ed3b1c78f47ff84149adbd586a18d71403d96ec24fec85c1"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.10.4/mini-agent-x86_64-unknown-linux-musl.tar.gz"
      sha256 "faa0aeb3aa36d1664d410f4586866345b0ef65696bfec5fcfb5eefaa313c9aab"
    else
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.10.4/mini-agent-aarch64-unknown-linux-musl.tar.gz"
      sha256 "35ee7936b67dce20eccd8be35f39aee2500f1fd05b8de6e77cbbad368236f182"
    end
  end

  def install
    bin.install "mini-agent"
    pkgshare.install "LICENSE", "NOTICE", "SOURCE.md"
    # Archives before 1.9.5 predate the third-party licence inventory.
    pkgshare.install "THIRD_PARTY_LICENSES" if File.exist?("THIRD_PARTY_LICENSES")
  end

  test do
    assert_match(/^mini-agent /, shell_output("#{bin}/mini-agent --version"))
  end
end
