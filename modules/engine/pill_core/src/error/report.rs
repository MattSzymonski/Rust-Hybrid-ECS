//! The miette presentation boundary: styled reports over plain errors.
//!
//! # Responsibilities
//!
//! - Wrap one engine error so a miette report prints its message with semantic
//!   terminal styling while keeping its codes, labels and sources.
//! - Install the engine's report handler as miette's process-wide hook, and
//!   turn one error into a report.
//!
//! # Design
//!
//! An error's own `Display` stays plain. Styling belongs to the reporting
//! boundary, so it lives in an adapter that is only ever constructed here.

use super::*;

// =============================================================================
// Miette Presentation
// =============================================================================

/// Presentation-only proxy that renders an engine error's primary message
/// with semantic terminal styling while keeping its diagnostic metadata.
///
/// The original error's `Display` stays plain; this adapter is allowed to
/// produce styled output because it is the reporting boundary, not the
/// domain error itself.
pub struct StyledDiagnosticProxy<E> {
    inner: E,
}

impl<E> StyledDiagnosticProxy<E> {
    /// Wrap an engine error for styled terminal reporting.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_core::error::{BuildError, StyledDiagnosticProxy};
    ///
    /// let proxy = StyledDiagnosticProxy::new(BuildError::Cancelled);
    /// assert!(matches!(proxy.into_inner(), BuildError::Cancelled));
    /// ```
    pub fn new(inner: E) -> Self {
        Self { inner }
    }

    /// Unwrap the original error.
    pub fn into_inner(self) -> E {
        self.inner
    }
}

impl<E> fmt::Display for StyledDiagnosticProxy<E>
where
    E: EngineMessage,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Step 1: Render the semantic message into a private buffer; the inner
        // scope ends the mutable borrow before the formatter is written.
        let mut buffer = String::new();
        {
            let mut renderer = TerminalMessageRenderer::new(&mut buffer);
            self.inner.render_message(&mut renderer)?;
        }
        // Step 2: Emit the finished buffer to the caller's formatter.
        formatter.write_str(&buffer)
    }
}

impl<E: fmt::Debug> fmt::Debug for StyledDiagnosticProxy<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.inner, formatter)
    }
}

impl<E> std::error::Error for StyledDiagnosticProxy<E>
where
    E: EngineMessage + miette::Diagnostic,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source()
    }
}

impl<E> miette::Diagnostic for StyledDiagnosticProxy<E>
where
    E: EngineMessage + miette::Diagnostic,
{
    fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.inner.code()
    }

    fn severity(&self) -> Option<miette::Severity> {
        self.inner.severity()
    }

    fn help<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.inner.help()
    }

    fn url<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.inner.url()
    }

    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        self.inner.source_code()
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = miette::LabeledSpan> + '_>> {
        self.inner.labels()
    }

    fn related<'a>(&'a self) -> Option<Box<dyn Iterator<Item = &'a dyn miette::Diagnostic> + 'a>> {
        self.inner.related()
    }

    fn diagnostic_source(&self) -> Option<&dyn miette::Diagnostic> {
        self.inner.diagnostic_source()
    }
}

/// Terminal-aware report handler: miette's graphical rendering over the
/// engine's diagnostic metadata.
///
/// The styled primary message comes from [`engine_report`], which wraps the
/// error in [`StyledDiagnosticProxy`] before it becomes a report.
#[derive(Debug)]
pub struct EngineReportHandler {
    inner: miette::GraphicalReportHandler,
}

impl Default for EngineReportHandler {
    fn default() -> Self {
        Self {
            inner: miette::GraphicalReportHandler::new(),
        }
    }
}

impl miette::ReportHandler for EngineReportHandler {
    fn debug(
        &self,
        error: &dyn miette::Diagnostic,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.inner.debug(error, formatter)
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Install the engine report handler as miette's process-wide hook.
///
/// Call once at the top-level reporting boundary, before any report is
/// printed.
pub fn install_engine_report_handler() {
    // Every report gets the engine handler.
    let _ = miette::set_hook(Box::new(
        |_error: &(dyn miette::Diagnostic + 'static)| -> Box<dyn miette::ReportHandler> {
            Box::new(EngineReportHandler::default())
        },
    ));
}

/// Convert one engine error into a miette report with semantic styling.
///
/// The report's primary message is the error's own semantic definition,
/// rendered for a terminal; miette keeps the codes, help, labels and sources.
pub fn engine_report<E>(error: E) -> miette::Report
where
    E: EngineMessage + miette::Diagnostic + Send + Sync + 'static,
{
    miette::Report::new(StyledDiagnosticProxy::new(error))
}
