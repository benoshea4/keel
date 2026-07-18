// hog-solver — phase 5 MLE fodder (see Cargo.toml header).

#[allow(warnings)]
mod bindings;

struct Component;

impl bindings::Guest for Component {
    fn solve(_input: String) -> Result<String, String> {
        let mut hoard: Vec<Vec<u8>> = Vec::new();
        loop {
            // 1 MiB per push; the vec is USED (len printed on the impossible
            // exit path) so the allocator can't elide it.
            hoard.push(vec![0xAB; 1024 * 1024]);
            if hoard.len() == usize::MAX {
                return Ok(hoard.len().to_string());
            }
        }
    }
}

bindings::export!(Component with_types_in bindings);
