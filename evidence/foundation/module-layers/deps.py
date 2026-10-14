"""Current graph: generated at inspection time from the Rust source, not hand edges."""
from pathlib import Path
import sys
import depsgx as d

root = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(root / "tools"))
from rust_module_graph import source_graph

graph = d.ModuleGraph(**source_graph(root))
