"""lldb formatters for rok programs built with `-g`.

lldb reads a DWARF variant part (a sum, an enum, an `any`) as a union of
every variant, `$variants$`, each `$variant$<discriminant>` holding the
discriminant `$discr$` and the variant's struct `value`. These show the
active variant only: its name as the summary, its fields as the children
(a boxed `any` payload through its box).

    command script import <sysroot>/lib/rok/lldb/rok_lldb.py

(`rokc --print lldbinit` prints that line.)
"""

import lldb


def is_sum(t, _dict):
    t = t.GetCanonicalType()
    return t.GetNumberOfFields() > 0 and t.GetFieldAtIndex(0).GetName() == "$variants$"


def active(v):
    """The active variant: its `$variant$<n>` member and the type of its
    `value`, or None. Read from the types and the bytes: walking the
    children here would leave the synthetic ones empty."""
    data = v.GetNonSyntheticValue().GetData()
    variants = v.GetType().GetCanonicalType().GetFieldAtIndex(0).GetType()
    err = lldb.SBError()
    for i in range(variants.GetNumberOfFields()):
        f = variants.GetFieldAtIndex(i)
        fields = [f.GetType().GetFieldAtIndex(k) for k in range(f.GetType().GetNumberOfFields())]
        by_name = {x.GetName(): x for x in fields}
        d, value = by_name.get("$discr$"), by_name.get("value")
        if d is None or value is None:
            continue
        at = f.GetOffsetInBytes() + d.GetOffsetInBytes()
        if d.GetType().GetByteSize() == 1:
            discr = data.GetUnsignedInt8(err, at)
        else:
            discr = data.GetUnsignedInt32(err, at)
        if err.Success() and f.GetName() == "$variant$%d" % discr:
            return f.GetName(), value.GetType()
    return None


def variant_name(sum_name, name):
    """`Shape::Rect` of `Shape` as `Rect`."""
    return name[len(sum_name) + 2 :] if name.startswith(sum_name + "::") else name


def child(v, c):
    """Field `c` of the active variant as a child of `v`; a pointer to a
    `box<T>` object as its value."""
    t = c.GetType()
    if t.IsPointerType() and t.GetPointeeType().GetName().startswith("box<") and c.GetValueAsUnsigned():
        b = c.Dereference().GetChildMemberWithName("value")
        if b.IsValid():
            return v.CreateValueFromAddress(c.GetName(), b.GetLoadAddress(), b.GetType())
    return v.CreateChildAtOffset(c.GetName(), c.GetLoadAddress() - v.GetLoadAddress(), t)


class Sum:
    def __init__(self, v, _dict):
        self.v = v
        self.kids = []

    def update(self):
        self.kids = []
        a = active(self.v)
        if a is not None:
            variants = self.v.GetNonSyntheticValue().GetChildAtIndex(0)
            value = variants.GetChildMemberWithName(a[0]).GetChildMemberWithName("value")
            self.kids = [
                child(self.v, value.GetChildAtIndex(i)) for i in range(value.GetNumChildren())
            ]
        return False

    def num_children(self):
        return len(self.kids)

    def get_child_index(self, name):
        for i, c in enumerate(self.kids):
            if c.GetName() == name:
                return i
        return -1

    def get_child_at_index(self, i):
        return self.kids[i] if 0 <= i < len(self.kids) else None

    def has_children(self):
        return bool(self.kids)


def summary(v, _dict):
    a = active(v)
    if a is None:
        return "<invalid discriminant>"
    return variant_name(v.GetType().GetCanonicalType().GetName(), a[1].GetName())


def __lldb_init_module(debugger, _dict):
    m = __name__
    debugger.HandleCommand(
        "type synthetic add -w rok -l %s.Sum --recognizer-function %s.is_sum" % (m, m)
    )
    debugger.HandleCommand(
        "type summary add -w rok -e -F %s.summary --recognizer-function %s.is_sum" % (m, m)
    )
    debugger.HandleCommand("type category enable rok")
