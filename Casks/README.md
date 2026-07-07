# Weft Homebrew Cask

## Local install (before official cask is merged)

```bash
# Build + sign + notarize first (produces Weft.app)
./scripts/build-app.sh
# or, with signing:
./scripts/notarize.sh

# Install via local cask
brew install --cask ./Casks/weft.rb

# Launch
open /Applications/Weft.app
```

## Update on new release

1. Update `version` and `sha256` in `weft.rb`
2. `sha256` is computed via `shasum -a 256 Weft-v1.0.0.zip`
3. The `url` field auto-fills from the version via string interpolation

## Submit to homebrew-cask (after first stable release)

Once v1.0.0 is published as a GitHub Release with the notarized ZIP:

```bash
brew tap homebrew/cask
cd "$(brew --repository)/Library/Taps/homebrew/homebrew-cask/Casks"
# Copy weft.rb here, then PR to homebrew-cask repo
```

The Cask will be available via `brew install --cask weft` after the PR is merged.
