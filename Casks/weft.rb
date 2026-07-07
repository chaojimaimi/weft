cask "weft" do
  version "1.0.0"
  sha256 :no_check # Replaced with actual hash on first release

  # Download URL — update once the first GitHub Release is published.
  # The release workflow uploads Weft-<tag>.zip as a release asset.
  url "https://github.com/chaojimaimi/weft/releases/download/v#{version}/Weft-v#{version}.zip"
  name "Weft"
  desc "Modern macOS terminal emulator with block-based command history"
  homepage "https://github.com/chaojimaimi/weft"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: ">= :monterey"

  app "Weft.app"

  zap trash: [
    "~/Library/Application Support/dev.weft.terminal",
    "~/Library/Preferences/dev.weft.terminal.plist",
    "~/Library/Caches/dev.weft.terminal",
    "~/Library/Saved Application State/dev.weft.terminal.savedState",
  ]
end
