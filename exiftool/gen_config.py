#!/usr/bin/env python3
"""Generate exiftool/ExifTool_config from the pan and copia ontologies.

Viewers that embed ExifTool (Xee³, XnView, digiKam …) show a namespace's
fields ONLY when the config declares each tag by name; a declared namespace
with undeclared tags shows an empty section. So every property in pan.ttl
and copia.ttl is declared here, and the file is regenerated whenever either
ontology changes:

    python3 exiftool/gen_config.py ontology/pan.ttl <path-to>/copia.ttl > exiftool/ExifTool_config
    cp exiftool/ExifTool_config ~/.ExifTool_config    # then RESTART the viewer

THE SHAPES COME FROM THE ONTOLOGY, NOT FROM A LIST (pan issue #32). A media
file's pan block nests two kinds of value, and both are read off pan.ttl:

  * an ObjectProperty whose range is a pan class is a STRUCT of that class's
    fields — the properties the class's cardinality blocks restrict, own and
    inherited (pan:Node gives every struct its id). It is ONE struct when the
    domain class restricts the property to at most one (pan:thumbnail on
    pan:Media), and an rdf:Bag of structs otherwise (the per-stage references:
    captionData, poseData, regionData, vectorData, depthData …).
  * an ObjectProperty whose range is outside pan (git-lex:Thing) is a plain
    bracket literal, one element per value (pan:id, pan:relatedToId).

A pan class that a property points at but that restricts nothing is a shape
this generator cannot describe: it exits non-zero naming the property, so a
new reference class is declared here the day it lands, not found months
later printing as a string.
"""
import re
import sys

PAN = "https://repolex.ai/ontology/pan/"
COPIA = "https://repolex.ai/ontology/copia/"

# The ONE exception list. Each entry names a pan property whose file shape
# the ontology does not carry, and why:
#   sceneObjects — a DatatypeProperty written as an rdf:Bag of strings, one
#                  member per object (pan.ttl: "Repeat the value, not the
#                  key"). OWL has no "multi-valued" mark; the comment is the
#                  only signal, and comments are not parsed.
LIST_TAGS = {"sceneObjects"}


def statements(path):
    """Turtle statements of the file, comment lines stripped, one per item."""
    lines = [l for l in open(path) if not l.lstrip().startswith("#")]
    text = "".join(lines)
    return [s.strip() for s in re.split(r"\s\.\s*\n", text) if s.strip()]


def read_ontology(path, prefix):
    """(properties, object property → (domain, range), class → parents,
    class → [restricted property, ...] in declaration order)."""
    props, obj, parents, restricted = [], {}, {}, {}
    for s in statements(path):
        head = s.split(None, 1)[0]
        if not head.startswith(prefix + ":"):
            continue
        name = head[len(prefix) + 1 :]
        if re.match(rf"^{prefix}:\S+ a owl:(Datatype|Object)Property\b", s):
            props.append(name)
            if " a owl:ObjectProperty" in s:
                dom = re.search(r"rdfs:domain\s+(\S+?)\s*(?:[;.]|$)", s)
                rng = re.search(r"rdfs:range\s+(\S+?)\s*(?:[;.]|$)", s)
                obj[name] = (
                    dom.group(1) if dom else None,
                    rng.group(1) if rng else None,
                )
        elif re.match(rf"^{prefix}:\S+ a owl:Class\b", s):
            m = re.search(r"rdfs:subClassOf\s+([^;]+)", s)
            parents.setdefault(name, [])
            if m:
                parents[name] += [p.strip() for p in m.group(1).split(",")]
        elif re.match(rf"^{prefix}:\S+ rdfs:subClassOf\s*\[", s):
            for p in re.findall(rf"owl:onProperty\s+{prefix}:(\w+)", s):
                restricted.setdefault(name, [])
                if p not in restricted[name]:
                    restricted[name].append(p)
    return sorted(set(props)), obj, parents, restricted


def read_max_one(path, prefix):
    """{class: {property}} — properties a class restricts to at most one."""
    out = {}
    for s in statements(path):
        m = re.match(rf"^{prefix}:(\S+) rdfs:subClassOf\s*\[", s)
        if not m:
            continue
        for p, kind in re.findall(
            rf"owl:onProperty\s+{prefix}:(\w+)\s*;\s*owl:(cardinality|maxCardinality)\s+1\b",
            s,
        ):
            out.setdefault(m.group(1), set()).add(p)
    return out


def struct_fields(cls, parents, restricted, prefix, seen=None):
    """Fields of a struct of class `cls`: ancestors' restricted properties
    first (root first), then its own, each once."""
    seen = seen or set()
    if cls in seen:
        return []
    seen.add(cls)
    fields = []
    for parent in parents.get(cls, []):
        if parent.startswith(prefix + ":"):
            for f in struct_fields(parent[len(prefix) + 1 :], parents, restricted, prefix, seen):
                if f not in fields:
                    fields.append(f)
    for f in restricted.get(cls, []):
        if f not in fields:
            fields.append(f)
    return fields


pan_ttl, copia_ttl = sys.argv[1], sys.argv[2]
pan, obj, parents, restricted = read_ontology(pan_ttl, "pan")
max_one = read_max_one(pan_ttl, "pan")
copia, _, _, _ = read_ontology(copia_ttl, "copia")

# Shape every object property whose range is a pan class.
structs = {}   # class local name → [fields]
shaped = {}    # property → (class, single?)
for prop, (dom, rng) in sorted(obj.items()):
    if not rng or not rng.startswith("pan:"):
        continue
    cls = rng[len("pan:") :]
    fields = struct_fields(cls, parents, restricted, "pan")
    # pan:Node hands every class an id; a class that restricts nothing of
    # its OWN (nor through a parent below Node) has no derivable shape.
    own = [f for f in fields if f not in restricted.get("Node", [])]
    if not own:
        sys.exit(
            f"gen_config.py: pan:{prop} points at pan:{cls}, which restricts no "
            f"property of its own, so its file shape cannot be derived. Give "
            f"pan:{cls} a cardinality block in pan.ttl (or change the range) and rerun."
        )
    structs[cls] = fields
    dom_cls = dom[len("pan:") :] if dom and dom.startswith("pan:") else None
    single = dom_cls is not None and prop in max_one.get(dom_cls, set())
    shaped[prop] = (cls, single)


def tag_lines(names, skip=()):
    return "\n".join(
        f"    {n} => {{ List => 'Bag' }}," if n in LIST_TAGS else f"    {n} => {{ }},"
        for n in names
        if n not in skip
    )


def struct_block(cls):
    fields = ", ".join(f"{f} => {{ }}" for f in structs[cls])
    return (
        f"%pan{cls} = (\n"
        f"    NAMESPACE   => {{ 'pan' => '{PAN}' }},\n"
        f"    STRUCT_NAME => 'Pan{cls}',\n"
        f"    {fields},\n"
        f");\n"
    )


def shaped_line(prop):
    cls, single = shaped[prop]
    if single:
        return f"    {prop} => {{ Struct => \\%pan{cls} }},"
    return f"    {prop} => {{ List => 'Bag', Struct => \\%pan{cls} }},"


print(f"""# GENERATED by exiftool/gen_config.py from pan.ttl and copia.ttl — do not edit.
# Teaches ExifTool (and every viewer that embeds it: Xee³, XnView, digiKam …)
# the Pan and Copia XMP vocabularies, each with its own family-0 group, so they
# show as "pan:Image properties" and "copia:Moment properties" beside
# "XMP properties". Every tag is declared by name: a viewer shows a declared
# namespace's fields only when the tag itself is declared. A file carries only
# these two namespaces (goodlux, 2026-09-07). The nested shapes (structs and
# reference bags) are derived from pan.ttl's cardinality blocks. Install:
#   cp exiftool/ExifTool_config ~/.ExifTool_config
# then RESTART the viewer (ExifTool reads this once per process).
""")
for cls in sorted(structs):
    print(struct_block(cls))

print(f"""%Image::ExifTool::UserDefined::pan = (
    GROUPS    => {{ 0 => 'pan:Image', 1 => 'pan:Image', 2 => 'Image' }},
    NAMESPACE => {{ 'pan' => '{PAN}' }},
    WRITABLE  => 'string',
{tag_lines(pan, shaped)}
{chr(10).join(shaped_line(p) for p in sorted(shaped))}
);

%Image::ExifTool::UserDefined::copia = (
    GROUPS    => {{ 0 => 'copia:Moment', 1 => 'copia:Moment', 2 => 'Image' }},
    NAMESPACE => {{ 'copia' => '{COPIA}' }},
    WRITABLE  => 'string',
{tag_lines(copia)}
);

%Image::ExifTool::UserDefined = (
    'Image::ExifTool::XMP::Main' => {{
        pan   => {{ SubDirectory => {{ TagTable => 'Image::ExifTool::UserDefined::pan' }} }},
        copia => {{ SubDirectory => {{ TagTable => 'Image::ExifTool::UserDefined::copia' }} }},
    }},
);
1;""")
