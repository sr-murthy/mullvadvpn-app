#!/usr/bin/env bash

# # Setup instructions before this script will work
#
# * Follow the instructions in ../README.md
# * Import and trust the GPG keys of everyone who the build server should trust code from
# * Set up an entry in `~/.ssh/config` for app-build-linux
# * Add the build servers public ssh key to the upload account on app-build-linux
#
# ## Windows
#
# * Add signtool.exe to your PATH: C:\Program Files (x86)\Windows Kits\10\bin\10.0.16299.0\x64

set -eu
shopt -s nullglob

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
BUILD_DIR="$SCRIPT_DIR/mullvadvpn-app"
# All non-dev builds have their artifacts placed under this directory
ARTIFACT_DIR="$SCRIPT_DIR/artifacts"
# Keeps track of which git commit hashes has been built, to not build them again
LAST_BUILT_DIR="$SCRIPT_DIR/last-built"

BRANCHES_TO_BUILD=("origin/main")

# shellcheck source=ci/buildserver-config.sh
source "$SCRIPT_DIR/buildserver-config.sh"

# Ask for the passphrase to the signing keys
case "$(uname -s)" in
    Darwin*)
        if [[ -z ${CSC_KEY_PASSWORD-} ]]; then
            read -rsp "CSC_KEY_PASSWORD = " CSC_KEY_PASSWORD
            echo ""
            export CSC_KEY_PASSWORD
        fi
        ;;
esac
# On macOS there is a separate key for signing the installer. See docs/macos-signing.md
case "$(uname -s)" in
    Darwin*)
        if [[ -z ${CSC_INSTALLER_KEY_PASSWORD-} ]]; then
            read -rsp "CSC_INSTALLER_KEY_PASSWORD = " CSC_INSTALLER_KEY_PASSWORD
            echo ""
            export CSC_INSTALLER_KEY_PASSWORD
        fi
        ;;
esac


# Automatically copy artifacts to the inbox of the repository builder service
# for dev and staging (production is pushed manually)
function publish_linux_repositories {
    local artifact_dir=$1
    local version=$2

    "$SCRIPT_DIR/publish-app-to-repositories.sh" --dev "$artifact_dir" "$version"

    # If this is a release build, also push to staging.
    # Publishing to production is done manually.
    if [[ $version != *"-dev-"* ]]; then
        "$SCRIPT_DIR/publish-app-to-repositories.sh" --staging "$artifact_dir" "$version"
    fi
}

# Uploads whatever matches the first argument to the Linux build server
function upload_sftp {
    echo "Uploading Mullvad VPN installers to app-build-linux:upload/"
    sftp app-build-linux <<EOF
cd upload
put $1
bye
EOF
}

function upload {
    local version=$1
    local files
    local checksums_filename

    checksums_filename="desktop+$(hostname)+$version.sha256"
    rm -f "$checksums_filename"
    files=( * )
    sha256sum "${files[@]}" > "$checksums_filename"

    case "$(uname -s)" in
        # Linux is both the build and upload server. Just copy directly to target dir
        Linux*)
            cp "${files[@]}" "$checksums_filename" "$UPLOAD_DIR/"
            ;;
        # Other platforms need to transfer their artifacts to the Linux build machine.
        Darwin*|MINGW*|MSYS_NT*)
            for file in "${files[@]}"; do
                upload_sftp "$file" || return 1
            done
            upload_sftp "$checksums_filename" || return 1
            ;;
    esac
}


# Run the arguments in an environment suitable for building the app. This
# means in a container on Linux, and straight up in the local shell elsewhere.
function run_in_build_env {
    if [[ "$(uname -s)" == "Linux" ]]; then
        USE_MOLD=false ./building/container-run.sh linux "$@"
    else
        bash -c "$*"
    fi
}

# Sign DEB+RPM on Linux
function sign_linux_packages {
    for installer_path in dist/MullvadVPN-*.deb; do
        echo "Signing $installer_path"
        dpkg-sig --sign builder "$installer_path"
    done
    for installer_path in dist/MullvadVPN-*.rpm; do
        echo "Signing $installer_path"
        rpm --addsign "$installer_path"
    done
}

# Builds the app and test artifacts and move them to the passed in `artifact_dir`.
# To cross compile pass in `target` as an environment variable
# to this function. Must also pass `artifact_dir` to show where to move the built artifacts.
# Pass all the build arguments as arguments to this function
function build {
    local target=${target:-""}
    local build_args=("${@}")

    run_in_build_env TARGETS="$target" ./build.sh "${build_args[@]}" || return 1
    if [[ "$(uname -s)" == "Linux" ]]; then
        sign_linux_packages
    fi
    mv dist/*.{deb,rpm,exe,pkg} "$artifact_dir" || return 1

    (run_in_build_env desktop/packages/mullvad-vpn/scripts/build-test-executable.sh \
        "$target" && \
        mv "dist/app-e2e-tests-$version"* "$artifact_dir") || \
        true
}

# Checks out the passed git reference passed to the working directory.
# Returns an error code if the commit/tag at `ref` is not properly signed.
function checkout_ref {
    ref=$1
    if [[ $ref == "refs/tags/"* ]] && ! git verify-tag "$ref"; then
        echo "!!!"
        echo "[#] $ref is a tag, but it failed GPG verification!"
        echo "!!!"
        return 1
    elif [[ $ref == "refs/remotes/"* ]] && ! git verify-commit "$current_hash"; then
        echo "!!!"
        echo "[#] $ref is a branch, but it failed GPG verification!"
        echo "!!!"
        return 1
    fi

    # Clean our working dir and check out the code we want to build
    rm -r dist/ 2&>/dev/null || true

    # Reset to main in case there is some issue on the current branch that prevents resetting to it.
    git reset --hard origin/main

    git checkout "$ref"

    # Return an error if it's not possible to reset to the current branch. Some errors will result in exit code 0 from `checkout` but >0 from `reset`.
    git reset --hard || return 1

    git submodule update
    git submodule update --init wireguard-go-rs/libwg/wireguard-go || true
    git clean -df
}

function build_ref {
    ref=$1
    tag=${2:-""}

    current_hash="$(git rev-parse "$ref^{commit}")"
    if [ -f "$LAST_BUILT_DIR/$current_hash" ]; then
        # This commit has already been built
        return 0
    fi

    echo ""
    echo "[#] $ref: $current_hash, building new packages."
    echo ""

    checkout_ref "$ref" || return 1

    # When we build in containers, the updating of toolchains is done by updating containers.
    if [[ "$(uname -s)" != "Linux" ]]; then
        echo "Updating Rust toolchain..."
        rustup update
    fi

    # podman appends a trailing carriage return to the output. So we use `tr` to strip it
    local version=""
    version="$(run_in_build_env cargo run -q --bin mullvad-version | tr -d "\r" || return 1)"

    local artifact_dir="$ARTIFACT_DIR/$version"
    mkdir -p "$artifact_dir"

    local build_args=(--optimize --sign)
    if [[ "$(uname -s)" == "Darwin" ]]; then
        build_args+=(--universal --notarize)
    fi
    if [[ "$(uname -s)" == "MINGW"* ]]; then
        # Check if the windows-installer crate is present, and if so, build a universal installer.
        # The check is needed for compatibility with older commits that don't have the crate/flag.
        # It was added in December 2024. The condition can be removed when supporting commits
        # older than that is no longer necessary.
        if [[ -d "$BUILD_DIR/windows-installer" ]]; then
            build_args+=(--universal)
        fi
    fi

    artifact_dir=$artifact_dir build "${build_args[@]}" || return 1
    if [[ "$(uname -s)" == "Linux" ]]; then
        echo "Building ARM64 installers"
        target=aarch64-unknown-linux-gnu artifact_dir=$artifact_dir build "${build_args[@]}" || return 1
    fi

    case "$(uname -s)" in
        MINGW*|MSYS_NT*)
            echo "Packaging all PDB files..."
            find ./windows/ \
                ./target/release/mullvad-daemon.pdb \
                ./target/release/mullvad.pdb \
                ./target/release/mullvad-problem-report.pdb \
                -iname "*.pdb" | tar -cJf "$artifact_dir/pdb-$version.tar.xz" -T -
            ;;
    esac

    # If there is a tag for this commit then we append that to the produced artifacts
    # A version suffix should only be created if there is a tag for this commit and it is not a release build
    if [[ -n "$tag" ]]; then
        # Remove disallowed version characters from the tag
        version_suffix="+${tag//[^0-9a-z_-]/}"
        # Will only match paths that include *-dev-* which means release builds will not be included
        # Pipes all matching names and their new name to mv
        pushd "$artifact_dir"
        for original_file in MullvadVPN-*-dev-*{.deb,.rpm,.exe,.pkg}; do
            new_file=$(echo "$original_file" | perl -pe "s/^(MullvadVPN-.*?)(_x64|_arm64|_aarch64|_amd64|_x86_64)?(\.deb|\.rpm|\.exe|\.pkg)$/\1$version_suffix\2\3/p")
            mv "$original_file" "$new_file"
        done
        popd

        if [[ $version == *"-dev-"* ]]; then
            version="$version$version_suffix"
        fi
    fi

    if [[ "$(uname -s)" == "Linux" ]]; then
        publish_linux_repositories "$artifact_dir" "$version"
    fi
    (cd "$artifact_dir" && upload "$version") || return 1
    # Remove artifacts from dev builds. They are not really needed and take up lots of space.
    if [[ $version == *"-dev-"* ]]; then
        # shellcheck disable=SC2216
        yes | rm -r "$artifact_dir"
    fi

    touch "$LAST_BUILT_DIR/$current_hash"

    echo ""
    echo "Successfully finished building $version at $(date)"
    echo ""
}

cd "$BUILD_DIR"

while true; do
    # Delete all tags. So when fetching we only get the ones existing on the remote
    git tag | xargs git tag -d > /dev/null

    git fetch --prune --tags 2> /dev/null || continue

    # Exclude android/ and ios/ tags from being built.
    # Tags can't include spaces so SC2207 isn't a problem here
    # shellcheck disable=SC2207
    tags=( $(git tag | grep -v -e "^android/.*\|^ios/.*") )

    for tag in "${tags[@]}"; do
        build_ref "refs/tags/$tag" "$tag" || echo "Failed to build tag $tag"
    done

    for branch in "${BRANCHES_TO_BUILD[@]}"; do
        build_ref "refs/remotes/$branch" || echo "Failed to build branch $tag"
    done

    sleep 240
done
