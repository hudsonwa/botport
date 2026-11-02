# Homebrew formula (draft)

`botport.rb` is a draft for a tap repo `hudsonwa/homebrew-tap` that **does not exist yet**. Once it does:

1. Tag a release (`v<version>`); `release.yml` publishes the four archives and `SHA256SUMS`.
2. Fill in the checksums from `SHA256SUMS`:

       v=0.1.2
       curl -fsSLO "https://github.com/hudsonwa/botport/releases/download/v$v/SHA256SUMS"
       cp packaging/homebrew/botport.rb /path/to/homebrew-tap/Formula/botport.rb
       sed -i '' -e "s/^  version \".*\"/  version \"$v\"/" /path/to/homebrew-tap/Formula/botport.rb
       while read -r sum file; do
         sed -i '' "s/REPLACE_WITH_SHA256_OF_$file/$sum/" /path/to/homebrew-tap/Formula/botport.rb
       done < SHA256SUMS

   (On Linux use `sed -i` without `''`.) `grep REPLACE_ /path/to/homebrew-tap/Formula/botport.rb` must print nothing.
3. Check it: `brew style Formula/botport.rb`, then `brew install --build-from-source hudsonwa/tap/botport && brew test botport`.

The archive's top-level `botport-<target>/` directory is stripped by Homebrew automatically, so `install` sees the
four binaries directly. The formula installs all four (`botport`, `grok2hermes`, `g2h`, `g2hd`), like `install.sh`.
