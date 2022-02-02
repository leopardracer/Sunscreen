#[derive(Debug, Clone, PartialEq)]
/**
 * Represents an error that can occur in this crate.
 */
pub enum Error {
    /**
     * When compiling an FHE program with the ParamsMode::Search option, you must specify a
     * PlainModulusConstraint.
     */
    MissingPlainModulusConstraint,

    /**
     * No parameters were found that satisfy the given FHE program.
     */
    NoParams,

    /**
     * Attempted to compile the given FHE program with the wrong scheme.
     */
    IncorrectScheme,

    /**
     * An internal error occurred in the SEAL library.
     */
    SealError(seal::Error),

    /**
     * An Error occurred in the Sunscreen runtime.
     */
    RuntimeError(crate::RuntimeError),
}

impl From<seal::Error> for Error {
    fn from(err: seal::Error) -> Self {
        Self::SealError(err)
    }
}

impl From<crate::RuntimeError> for Error {
    fn from(err: crate::RuntimeError) -> Self {
        Self::RuntimeError(err)
    }
}

/**
 * Wrapper around [`Result`](std::result::Result) with this crate's error type.
 */
pub type Result<T> = std::result::Result<T, Error>;
