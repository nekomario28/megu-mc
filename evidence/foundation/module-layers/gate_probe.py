"""The inspector must detect every component and preserve all fault kinds."""
import depsgx as d

graph = d.ModuleGraph(
    modules=["upper", "lower", "a", "b"],
    depends=[("upper", "lower"), ("lower", "upper"), ("a", "b"), ("b", "a")],
    roots=["upper", "a"],
    layers=[["upper"], ["lower", "a", "b"]],
    forbidden=[("lower", "upper")],
)
