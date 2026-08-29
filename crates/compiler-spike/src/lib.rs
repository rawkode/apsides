//! M0 proof that the Rust Oxc pipeline can lower TypeScript and JSX.
//!
//! This crate is deliberately not a bundler or type checker. M0 uses Rolldown
//! for bundling and TypeScript's own checker as separate stages.

use std::{mem, path::Path};

use oxc::{
    CompilerInterface,
    codegen::CodegenReturn,
    diagnostics::Diagnostics,
    span::SourceType,
    transformer::{JsxOptions, JsxRuntime, TransformOptions},
};
use thiserror::Error;

/// A failed Oxc transformation.
#[derive(Debug, Error)]
pub enum TransformError {
    /// The input filename is not a supported JavaScript/TypeScript source type.
    #[error("unsupported source path: {0}")]
    UnsupportedSource(String),
    /// Oxc rejected the source program.
    #[error("Oxc rejected the source with {count} diagnostic(s):\n{rendered}")]
    Diagnostics { count: usize, rendered: String },
}

/// Transform one self-contained TSX module with Oxc's automatic JSX runtime.
///
/// # Errors
///
/// Returns [`TransformError`] when the path does not identify a supported
/// source type or Oxc reports parser or transformation diagnostics.
pub fn transform_tsx(
    source_path: &Path,
    source: &str,
    jsx_import_source: &str,
) -> Result<String, TransformError> {
    let source_type = SourceType::from_path(source_path)
        .map_err(|_| TransformError::UnsupportedSource(source_path.display().to_string()))?;
    let mut compiler = ConfiguredCompiler::new(jsx_import_source);
    compiler
        .execute(source, source_type, source_path)
        .map_err(|diagnostics| {
            let count = diagnostics.len();
            let rendered = diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.render_with_source_code(source.to_owned()))
                .collect::<Vec<_>>()
                .join("\n");
            TransformError::Diagnostics { count, rendered }
        })
}

struct ConfiguredCompiler {
    options: TransformOptions,
    printed: String,
    errors: Diagnostics,
}

impl ConfiguredCompiler {
    fn new(jsx_import_source: &str) -> Self {
        let options = TransformOptions {
            jsx: JsxOptions {
                runtime: JsxRuntime::Automatic,
                import_source: Some(jsx_import_source.to_owned()),
                ..JsxOptions::default()
            },
            ..TransformOptions::default()
        };
        Self {
            options,
            printed: String::new(),
            errors: Diagnostics::new(),
        }
    }

    fn execute(
        &mut self,
        source: &str,
        source_type: SourceType,
        source_path: &Path,
    ) -> Result<String, Diagnostics> {
        self.compile(source, source_type, source_path);
        if self.errors.is_empty() {
            Ok(mem::take(&mut self.printed))
        } else {
            Err(mem::take(&mut self.errors))
        }
    }
}

impl CompilerInterface for ConfiguredCompiler {
    fn handle_errors(&mut self, errors: Diagnostics) {
        self.errors.extend(errors);
    }

    fn transform_options(&self) -> Option<&TransformOptions> {
        Some(&self.options)
    }

    fn after_codegen(&mut self, output: CodegenReturn<'_>) {
        self.printed = output.code;
    }
}

#[cfg(test)]
mod tests {
    use super::transform_tsx;
    use std::path::Path;

    #[test]
    fn lowers_typescript_and_automatic_jsx_in_rust() {
        let source = r#"
            type Props = { name: string };
            export function Greeting({ name }: Props) {
              return <ConfigMap name={name}><Data key="mode" value="safe" /></ConfigMap>;
            }
        "#;

        let output = transform_tsx(
            Path::new("fixture.tsx"),
            source,
            "@oxideloop/react-like-runtime",
        )
        .expect("valid TSX should transform");

        assert!(!output.contains("type Props"), "TypeScript must be erased");
        assert!(
            output.contains("@oxideloop/react-like-runtime/jsx-runtime"),
            "automatic JSX import must use the configured runtime: {output}"
        );
        assert!(!output.contains("<ConfigMap"), "JSX syntax must be lowered");
    }

    #[test]
    fn rejects_invalid_tsx() {
        let error = transform_tsx(
            Path::new("broken.tsx"),
            "export const broken = <Deployment>",
            "@oxideloop/react-like-runtime",
        )
        .expect_err("unclosed JSX must fail");

        assert!(error.to_string().contains("diagnostic"));
    }
}
