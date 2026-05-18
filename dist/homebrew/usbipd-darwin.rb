# Reference Homebrew formula for usbipd-darwin.
#
# Copy this file into the `homebrew-usbipd-darwin` tap repo as
# `Formula/usbipd-darwin.rb` for the first release, then bump
# `version`, `url`, and `sha256` per release. See ../README.md for the
# end-to-end release flow.
class UsbipdDarwin < Formula
  desc      "USB/IP server for macOS — exposes host USB devices to Linux guests"
  homepage  "https://github.com/lima-vm/usbipd-darwin"
  version   "0.1.0"
  license   any_of: ["Apache-2.0", "MIT"]

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/lima-vm/usbipd-darwin/releases/download/v#{version}/usbipd-darwin-#{version}-aarch64-apple-darwin.tar.gz"
      # Replace with the contents of the matching .sha256 file from the GitHub Release.
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/lima-vm/usbipd-darwin/releases/download/v#{version}/usbipd-darwin-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  depends_on :macos
  depends_on macos: :sonoma # 14+

  def install
    bin.install "usbipd"
    doc.install "README.md", "CHANGELOG.md", "SECURITY.md", "LICENSE-APACHE", "LICENSE-MIT"
    (share/"usbipd-darwin").install "launchd"
  end

  def caveats
    <<~EOS
      usbipd needs root to force-capture USB devices (see README.md).

      To allow your admin user to run it without a password prompt, run:
        sudo "#{opt_bin}/usbipd" sudoers | sudo tee /etc/sudoers.d/usbipd-darwin
        sudo chmod 0440 /etc/sudoers.d/usbipd-darwin

      A sample launchd plist is installed at:
        #{opt_share}/usbipd-darwin/launchd/io.usbipd.daemon.plist

      Edit it to set your access policy (--allow VID:PID ...), copy it to
      /Library/LaunchDaemons/, and load with:
        sudo launchctl bootstrap system /Library/LaunchDaemons/io.usbipd.daemon.plist

      Security model and Lima integration are documented at:
        https://github.com/lima-vm/usbipd-darwin#readme
    EOS
  end

  test do
    # Subcommand smoke test — no privileged operations.
    assert_match "usbipd #{version}", shell_output("#{bin}/usbipd --version")
    # `list` exits 0 with no devices, fails with a parse error otherwise; we
    # only check it doesn't crash.
    system bin/"usbipd", "list", "--help"
  end
end
