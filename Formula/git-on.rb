class GitOn < Formula
  desc "Run commands on a specific Git revision"
  homepage "https://github.com/progbeat/git-on"
  version "1.5.1"
  license "MIT"

  on_macos do
    depends_on arch: :arm64
    depends_on macos: :ventura

    url "https://github.com/progbeat/git-on/releases/download/v#{version}/git-on-aarch64-apple-darwin.tar.gz"
    sha256 "882a90e6ea2c6ca160012970ceaad994c6ea68a40dce820a4e670636035fc1f7"
  end

  on_linux do
    depends_on arch: :x86_64

    url "https://github.com/progbeat/git-on/releases/download/v#{version}/git-on-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "32bb26d7fa79dac1b1df2058d442318f3bc3dd118d67d1988784b6bc6f5b79aa"
  end

  def install
    bin.install "git-on"
    pkgshare.install "LICENSE", "THIRD_PARTY_NOTICES"
  end

  test do
    assert_match "git-on #{version}", shell_output("#{bin}/git-on --version")
  end
end
