class Zerostack < Formula
  desc "Minimalistic coding agent written in Rust, optimized for memory footprint and performance"
  homepage "https://github.com/sebahrens/mini-agent"
  version "1.9.4"
  license "GPL-3.0-only"

  on_macos do
    if Hardware::CPU.intel?
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.9.4/mini-agent-x86_64-apple-darwin.tar.gz"
      sha256 "aa8ec365106c57492d43471834386e6cb60dd42fd4de31f1845332455a353d71"
    else
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.9.4/mini-agent-aarch64-apple-darwin.tar.gz"
      sha256 "23bf276e5e13782539dae47c8765b713d9d559c4f187857aeb288628980c404f"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.9.4/mini-agent-x86_64-unknown-linux-musl.tar.gz"
      sha256 "ab9feae6f7b7ffe3de03b8068847651c564671c3c74782ba5141f308d6653cd0"
    else
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.9.4/mini-agent-aarch64-unknown-linux-musl.tar.gz"
      sha256 "2cd1fbcbc593456ae4eaac08a37005985d54c8261572ac31ef6a44e1f459141c"
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
