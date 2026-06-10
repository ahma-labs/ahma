use std::path::Path;

/// Checks if the file being written already exists.
/// If it does, returns a helpful error guiding the model to use replace_in_file instead.
pub fn check_write_allowance(path: &Path) -> Result<(), String> {
    if path.exists() {
        Err("FILE_EXISTS: Use replace_in_file for existing files. write_file is for new files only.\n\
             Hint: Call replace_in_file with old_str/new_str to edit the specific section."
            .to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_write_guard() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("existing.txt");
        std::fs::write(&file_path, "hello").unwrap();

        assert!(check_write_allowance(&file_path).is_err());

        let new_file_path = dir.path().join("new.txt");
        assert!(check_write_allowance(&new_file_path).is_ok());
    }
}
