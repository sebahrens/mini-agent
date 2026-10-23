class Zerostack < Formula
  desc "Minimalistic coding agent written in Rust, optimized for memory footprint and performance"
  homepage "https://github.com/sebahrens/mini-agent"
  version "1.10.2"
  license "GPL-3.0-only"

  on_macos do
    if Hardware::CPU.intel?
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.10.2/mini-agent-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.10.2/mini-agent-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.10.2/mini-agent-x86_64-unknown-linux-musl.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.10.2/mini-agent-aarch64-unknown-linux-musl.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
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
