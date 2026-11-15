# What berth hands to Ansible

Each `NAME.profile.json` is a made-up host's profile, and `NAME.vars.json` the variables berth gives the role for it (`apply::ansible_vars`). A test in `crates/berth-core/src/apply.rs` compares the two. Write the variables again with `BERTH_UPDATE_DOCS=1 cargo test -p berth-core ansible_contract`.

CI runs the end-to-end tests that apply the role on a pull request only when these files, the role or the code in `SEAM` of `ci/plan.py` change. A change to what the role gets shows here, so it runs them.
