"""Capability registry — exports all available security capabilities."""

from __future__ import annotations

from .code_scan import CodeScanCapability

ALL_CAPABILITIES = [CodeScanCapability()]
