# Versioning and Auto-Update

> [!NOTE]
> Semantic Versioning was adopted by Subnet 2 starting from version 1.0.0.

This project uses Semantic Versioning (SemVer) for version numbering and includes an auto-update feature to ensure users are running the latest version.

## Semantic Versioning

We follow the Semantic Versioning 2.0.0 specification (https://semver.org/). Our version numbers take the form of MAJOR.MINOR.PATCH, where:

1. MAJOR version increments indicate incompatible API changes
2. MINOR version increments indicate new functionality in a backwards-compatible manner
3. PATCH version increments indicate backwards-compatible bug fixes

## Auto-Update Feature

The project includes an auto-update utility (`AutoUpdate` class in `neurons/utils.py`) that performs the following tasks:

1. Checks the remote repository for a newer version
2. Compares the remote version with the local version
3. Automatically updates the local repository if a newer version is available
4. Handles potential merge conflicts
5. Updates package dependencies if necessary

### Version Checking

The auto-update feature compares the `__version__` string in the local and remote `neurons/__init__.py` files. It converts these version strings to integers for comparison (e.g., "1.2.3" becomes 123).

### Update Process

If a newer version is detected, the auto-update feature:

1. Pulls the latest changes from the remote repository
2. Attempts to resolve any merge conflicts automatically
3. Updates package dependencies if the `requirements.txt` file has changed
4. Restarts the application to apply the updates

## Manual Updates

While the auto-update feature is designed to keep the application up-to-date automatically, users can also perform manual updates by pulling the latest changes from the repository and updating their dependencies.

```bash
git fetch origin
git checkout main
git pull origin main
pip install -r requirements.txt
pm2 restart all
```

## Version History

For a detailed changelog of version updates, please refer to [the releases section of the repository] or [release notes on Subnet 2's GitBook].

[the releases section of the repository](https://github.com/inference-labs-inc/subne-2t/releases)
[release notes on Subnet 2's GitBook](https://sn2-docs.inferencelabs.com/release-notes)
