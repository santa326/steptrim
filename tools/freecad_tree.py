# Dump the product tree FreeCAD builds from a STEP file, as JSON, for comparison with stepinfo.
# Run with:  FreeCADCmd.exe -c "exec(open('tools/freecad_tree.py').read())"  with STEP_IN / JSON_OUT env vars set.
import os, sys, json, time
import FreeCAD as App
import Import

src = os.environ["STEP_IN"]
out = os.environ["JSON_OUT"]
t0 = time.time()
doc = App.newDocument("cmp")
Import.insert(src, doc.Name)
t1 = time.time()

def node(obj):
    kids = []
    if hasattr(obj, "Group"):
        for k in obj.Group:
            kids.append(node(k))
    elif obj.TypeId == "App::Link" and obj.LinkedObject is not None:
        kids.append(node(obj.LinkedObject))
    return {"label": obj.Label, "type": obj.TypeId, "children": kids}

# roots = objects nobody contains
contained = set()
for o in doc.Objects:
    if hasattr(o, "Group"):
        for k in o.Group:
            contained.add(k.Name)
roots = [o for o in doc.Objects if o.Name not in contained]

def count(n):
    return 1 + sum(count(c) for c in n["children"])

tree = [node(r) for r in roots]
result = {
    "file": src,
    "import_seconds": round(t1 - t0, 2),
    "objects": len(doc.Objects),
    "roots": len(roots),
    "instances": sum(count(t) for t in tree),
    "tree": tree,
}
with open(out, "w", encoding="utf-8") as f:
    json.dump(result, f, indent=1, ensure_ascii=False)
print("wrote", out, "objects", len(doc.Objects), "roots", len(roots), "import_s", round(t1 - t0, 2))
