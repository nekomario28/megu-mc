# SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
"""ctypes bindings for the ABI v1 entry points used by the wire probe."""
import ctypes as C
import subprocess
from contextlib import contextmanager

from protocol767 import require

ABI_VERSION = 1
SECTION_CELLS = 4096
OK, OUT_OF_RANGE, INVALID_STATE, INVALID_NBT = 0, 2, 4, 5
BYTE_BUDGET, INVALID_GZIP, BUFFER_TOO_SMALL = 7, 8, 10
HORIZONTAL, BELOW, STEPPED_UP = 1, 4, 8
WATER, PUSHED, PLAYER = 1, 2, 4
FALLING, BLOCKS_MOTION, ALL_FACES = 1, 2, 0x3F
Vec = C.c_double * 3
Handle = C.c_void_p
U8, U16, U32, U64 = C.c_uint8, C.c_uint16, C.c_uint32, C.c_uint64
Size, Status, Ptr = C.c_size_t, C.c_int32, C.POINTER


class Bytes(C.Structure):
    _fields_ = [("data", Ptr(U8)), ("len", U64)]


class Text(C.Structure):
    _fields_ = Bytes._fields_


class CommandParse(C.Structure):
    _fields_ = [("cursor", U64), ("executable", U8)]


class SuggestionInfo(C.Structure):
    _fields_ = [("start", U64), ("end", U64), ("count", U64)]


class Aabb(C.Structure):
    _fields_ = [("min", Vec), ("max", Vec)]


class Collision(C.Structure):
    _fields_ = [("delta", Vec), ("flags", U8)]


class FluidInput(C.Structure):
    _fields_ = [("velocity", Vec), ("kind", U8), ("flags", U8)]


class FluidFacts(C.Structure):
    _fields_ = [("kind", U8), ("amount", U8), ("flags", U8), ("sturdy_faces", U8)]


class FluidResult(C.Structure):
    _fields_ = [("push", Vec), ("max_height", C.c_double), ("in_fluid", U8)]


Lookup = C.CFUNCTYPE(Status, Handle, Status, Status, Status, Ptr(FluidFacts))

RECORDS = {
    "bytes": Bytes,
    "text": Text,
    "command_parse": CommandParse,
    "suggestion_info": SuggestionInfo,
    "aabb": Aabb,
    "collision": Collision,
    "fluid_input": FluidInput,
    "fluid_facts": FluidFacts,
    "fluid_result": FluidResult,
}


def compile_c(include_dir, output, source, *flags):
    subprocess.run(
        ["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pedantic",
         "-I", str(include_dir), *flags, "-x", "c", "-", "-o", str(output)],
        input=source, text=True, check=True,
    )


def verify_layout(include_dir, temporary_dir):
    """Compare size, alignment and offsets to the public C11 header."""
    expressions = ["MC_SPINE_KERNEL_ABI_VERSION"]
    expected = [ABI_VERSION]
    for name, record in RECORDS.items():
        c_type = f"mc_spine_kernel_{name}"
        expressions.extend((f"sizeof({c_type})", f"_Alignof({c_type})"))
        expected.extend((C.sizeof(record), C.alignment(record)))
        for field, field_type in record._fields_:
            expressions.extend((f"offsetof({c_type}, {field})", f"sizeof((({c_type} *)0)->{field})"))
            expected.extend((getattr(record, field).offset, C.sizeof(field_type)))
    source = (
        '#include "mc_spine_kernel.h"\n#include <stdio.h>\nint main(void) {\n'
        + "".join(f'    printf("%zu\\n", (size_t)({expr}));\n' for expr in expressions)
        + "    return 0;\n}\n"
    )
    probe = temporary_dir / "abi-layout"
    compile_c(include_dir, probe, source)
    actual = list(map(int, subprocess.check_output([probe], text=True).split()))
    require(len(actual) == len(expected), "c-abi-layout-probe-length")
    for expr, got, want in zip(expressions, actual, expected):
        require(got == want, f"c-abi-layout {expr}: C={got}, ctypes={want}")


class Kernel:
    def __init__(self, path):
        self.lib = C.CDLL(str(path))
        version = self.lib.mc_spine_kernel_abi_version
        version.argtypes, version.restype = [], U32
        actual_version = version()
        require(actual_version == ABI_VERSION,
                f"c-abi-version: library={actual_version}, expected={ABI_VERSION}")

        signatures = {
            "bytes_free": (Bytes,),
            "palette_new_single": (U32, Ptr(Handle)),
            "palette_free": (Handle,),
            "palette_get": (Handle, U32, Ptr(U32)),
            "palette_set": (Handle, U32, U32),
            "palette_non_air": (Handle, Ptr(U16)),
            "palette_unpack_into": (Handle, Ptr(U32), Size),
            "palette_write_block_states_wire": (Handle, Ptr(Bytes)),
            "nbt_read_named_validate": (C.c_char_p, Size),
            "nbt_read_gzip_validate": (C.c_char_p, Size, Size),
            "nbt_roundtrip_named": (C.c_char_p, Size, Ptr(Bytes)),
            "dispatcher_new": (Ptr(Handle),),
            "dispatcher_free": (Handle,),
            "dispatcher_register_literal": (Handle, C.c_char_p, Size),
            "dispatcher_parse": (Handle, C.c_char_p, Size, Ptr(CommandParse)),
            "dispatcher_complete_suggestions": (Handle, C.c_char_p, Size, Size, Ptr(Handle)),
            "suggestions_info": (Handle, Ptr(SuggestionInfo)),
            "suggestions_get": (Handle, Size, Ptr(Text)),
            "suggestions_free": (Handle,),
            "collide": (Ptr(Aabb), Ptr(C.c_double), U8, C.c_float, Ptr(Aabb), Size, Ptr(Collision)),
            "fluid_impulse": (Ptr(Aabb), Ptr(FluidInput), Lookup, Handle, Ptr(FluidResult)),
        }
        self.functions = {}
        for name, args in signatures.items():
            function = getattr(self.lib, "mc_spine_kernel_" + name)
            function.argtypes, function.restype = args, Status
            self.functions[name] = function

    def call(self, name, *args, status=OK):
        actual = self.functions[name](*args)
        require(actual == status, f"c-abi-{name}: status={actual}, expected={status}")

    @contextmanager
    def owned(self, create, free, *args):
        handle = Handle()
        self.call(create, *args, C.byref(handle))
        try:
            yield handle
        finally:
            self.call(free, handle)

    def bytes(self, name, *args):
        output = Bytes()
        self.call(name, *args, C.byref(output))
        try:
            return C.string_at(output.data, output.len)
        finally:
            self.call("bytes_free", output)
