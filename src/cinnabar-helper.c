/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cache.h"
#include "attr.h"
#include "blob.h"
#include "commit.h"
#include "config.h"
#include "diff.h"
#include "diffcore.h"
#include "exec-cmd.h"
#include "hashmap.h"
#include "log-tree.h"
#include "shallow.h"
#include "strslice.h"
#include "strbuf.h"
#include "string-list.h"
#include "streaming.h"
#include "object.h"
#include "oidset.h"
#include "quote.h"
#include "refs.h"
#include "remote.h"
#include "replace-object.h"
#include "revision.h"
#include "run-command.h"
#include "tree.h"
#include "tree-walk.h"
#include "hg-data.h"
#include "cinnabar-helper.h"
#include "cinnabar-fast-import.h"
#include "cinnabar-notes.h"

#define _STRINGIFY(s) # s
#define STRINGIFY(s) _STRINGIFY(s)

struct notes_tree git2hg, hg2git, files_meta;
struct object_id metadata_oid, changesets_oid, manifests_oid, git2hg_oid,
                 hg2git_oid, files_meta_oid;

// XXX: Should use a hg-specific oidset type.
struct oidset hg2git_seen = OIDSET_INIT;

int metadata_flags = 0;

struct iter_tree_context {
	void *ctx;
	iter_tree_cb callback;
	struct object_list *list;
	int recursive;
};

static int do_iter_tree(const struct object_id *oid, struct strbuf *base,
			const char *pathname, unsigned mode, void *context)
{
	struct iter_tree_context *ctx = context;

	if (S_ISDIR(mode)) {
		object_list_insert((struct object *)lookup_tree(the_repository, oid),
		                   &ctx->list);
		if (ctx->recursive)
			return READ_TREE_RECURSIVE;
	}

	ctx->callback(oid, base, pathname, mode, ctx->ctx);
	return 0;
}

int iter_tree(const struct object_id *oid, iter_tree_cb callback, void *context, int recursive) {
	struct tree *tree = NULL;
	struct iter_tree_context ctx = { context, callback, NULL, recursive };
	struct pathspec match_all;

	tree = parse_tree_indirect(oid);
	if (!tree)
		return 0;

	memset(&match_all, 0, sizeof(match_all));
	read_tree(the_repository, tree, &match_all, do_iter_tree, &ctx);

	while (ctx.list) {
		struct object *obj = ctx.list->item;
		struct object_list *elem = ctx.list;
		ctx.list = elem->next;
		free(elem);
		free_tree_buffer((struct tree *)obj);
	}
	return 1;
}

struct object_id *commit_oid(struct commit *c) {
	return &c->object.oid;
}

struct rev_info *rev_list_new(int argc, const char **argv) {
	struct rev_info *revs = xmalloc(sizeof(*revs));

	init_revisions(revs, NULL);
	// Note: we do a pass through, but don't make much effort to actually
	// support all the options properly.
	setup_revisions(argc, argv, revs, NULL);

	if (prepare_revision_walk(revs))
		die("revision walk setup failed");

	return revs;
}

void rev_list_finish(struct rev_info *revs) {
	// More extensive than reset_revision_walk(). Otherwise --boundary
	// and pathspecs don't work properly.
	clear_object_flags(ALL_REV_FLAGS | TOPO_WALK_EXPLORED | TOPO_WALK_INDEGREE);
	release_revisions(revs);
	free(revs);
}

int maybe_boundary(struct rev_info *revs, struct commit *commit) {
	struct commit_list *parent;
	struct commit_graft *graft;

	if (commit->object.flags & BOUNDARY)
		return 1;

	parent = commit->parents;
	if (revs->boundary && !parent &&
		is_repository_shallow(the_repository) &&
		(graft = lookup_commit_graft(
			the_repository, &commit->object.oid)) != NULL &&
		graft->nr_parent < 0) {
		return 2;
	}
	return 0;
}

struct diff_tree_file {
	struct object_id *oid;
	char *path;
	unsigned short mode;
};

struct diff_tree_item {
	struct diff_tree_file a;
	struct diff_tree_file b;
	unsigned short int score;
	char status;
};

struct diff_tree_ctx {
	void (*cb)(void *, struct diff_tree_item *);
	void *context;
};

static void diff_tree_cb(struct diff_queue_struct *q,
                         struct diff_options *opt, void *data)
{
	struct diff_tree_ctx *ctx = data;
	int i;

	for (i = 0; i < q->nr; i++) {
		struct diff_filepair *p = q->queue[i];
		if (p->status == 0)
			die("internal diff status error");
		if (p->status != DIFF_STATUS_UNKNOWN) {
			struct diff_tree_item item = {
				{ &p->one->oid, p->one->path, p->one->mode },
				{ &p->two->oid, p->two->path, p->two->mode },
				p->score,
				p->status,
			};
			ctx->cb(ctx->context, &item);
		}
	}
}

void diff_tree_(int argc, const char **argv, void (*cb)(void *, struct diff_tree_item *), void *context)
{
	struct diff_tree_ctx ctx = { cb, context };
	struct rev_info revs;

	init_revisions(&revs, NULL);
	revs.diff = 1;
	// Note: we do a pass through, but don't make much effort to actually
	// support all the options properly.
	setup_revisions(argc, argv, &revs, NULL);
	revs.diffopt.output_format = DIFF_FORMAT_CALLBACK;
	revs.diffopt.format_callback = diff_tree_cb;
	revs.diffopt.format_callback_data = &ctx;
	revs.diffopt.flags.recursive = 1;

	if (revs.pending.nr != 2)
		die("diff-tree needs two revs");

	diff_tree_oid(&revs.pending.objects[0].item->oid,
	              &revs.pending.objects[1].item->oid,
	              "", &revs.diffopt);
	log_tree_diff_flush(&revs);
	release_revisions(&revs);
}

void ensure_notes(struct notes_tree *notes)
{
	if (!notes_initialized(notes)) {
		const struct object_id *oid;
		int flags = 0;
		if (notes == &git2hg)
			oid = &git2hg_oid;
		else if (notes == &hg2git)
			oid = &hg2git_oid;
		else if (notes == &files_meta) {
			oid = &files_meta_oid;
			if (!(metadata_flags & FILES_META))
				flags = NOTES_INIT_EMPTY;
		} else
			die("Unknown notes tree");
		if (is_null_oid(oid))
			flags = NOTES_INIT_EMPTY;
		init_notes(notes, oid_to_hex(oid), combine_notes_ignore, flags);
	}
}

const struct object_id *repo_lookup_replace_object(
	struct repository *r, const struct object_id *oid)
{
	return lookup_replace_object(r, oid);
}

const struct object_id *resolve_hg(
	struct notes_tree* tree, const struct hg_object_id *oid, size_t len)
{
	struct object_id git_oid;
	const struct object_id *note;

	ensure_notes(tree);

	note = get_note_hg(tree, oid);
	if (len == 40)
		return note;

	hg_oidcpy2git(&git_oid, oid);
	return get_abbrev_note(tree, &git_oid, len);
}

const struct object_id *resolve_hg2git(const struct hg_object_id *oid,
                                       size_t len)
{
	return resolve_hg(&hg2git, oid, len);
}

/* The git storage for a mercurial manifest uses not-entirely valid file modes
 * to keep the mercurial manifest data as git trees.
 * While mercurial manifests are flat, the corresponding git tree uses
 * sub-directories. The file sha1s are stored as git links (since they're not
 * valid git sha1s), and the file modes are stored as extra bits in the git
 * link file mode, that git normally ignores.
 * - Symlinks are set to have a file mode of 0160000 (standard git link).
 * - Executables are set to have a file mode of 0160755.
 * - Regular files are set to have a file mode of 0160644.
 */

/* Return the mercurial manifest character corresponding to the given
 * git file mode. */
static const char *hgattr(unsigned int mode)
{
	if (S_ISGITLINK(mode)) {
		if ((mode & 0755) == 0755)
			return "x";
		else if ((mode & 0644) == 0644)
			return "";
		else if ((mode & 0777) == 0)
			return "l";
	}
	die("Unsupported mode %06o", mode);
}

/* The git storage for a mercurial manifest used to be a commit with two
 * directories at its root:
 * - a git directory, matching the git tree in the git commit corresponding to
 *   the mercurial changeset using the manifest.
 * - a hg directory, containing the same file paths, but where all pointed
 *   objects are commits (mode 160000 in the git tree) whose sha1 is actually
 *   the mercurial sha1 for the corresponding mercurial file.
 * Reconstructing the mercurial manifest required file paths, mercurial sha1
 * for each file, and the corresponding attribute ("l" for symlinks, "x" for
 * executables"). The hg directory alone was not enough for that, because it
 * lacked the attribute information.
 */
static void track_tree(struct tree *tree, struct object_list **tree_list)
{
	if (tree_list) {
		object_list_insert(&tree->object, tree_list);
		tree->object.flags |= SEEN;
	}
}

struct manifest_tree_state {
	struct tree *tree;
	struct tree_desc desc;
};

static int manifest_tree_state_init(const struct object_id *tree_id,
                                    struct manifest_tree_state *result,
                                    struct object_list **tree_list)
{
	result->tree = parse_tree_indirect(tree_id);
	if (!result->tree)
		return -1;
	track_tree(result->tree, tree_list);

	init_tree_desc(&result->desc, result->tree->buffer,
	               result->tree->size);
	return 0;
}

struct merge_manifest_tree_state {
	struct manifest_tree_state state_a, state_b;
	struct name_entry entry_a, entry_b;
	struct strslice entry_a_path, entry_b_path;
	int cmp;
};

struct merge_name_entry {
	struct name_entry *entry_a, *entry_b;
	struct strslice path;
};

static int merge_manifest_tree_state_init(const struct object_id *tree_id_a,
                                          const struct object_id *tree_id_b,
                                          struct merge_manifest_tree_state *result,
                                          struct object_list **tree_list)
{
	int ret;
	memset(result, 0, sizeof(*result));
	result->cmp = 0;

	if (tree_id_a) {
		ret = manifest_tree_state_init(tree_id_a, &result->state_a, tree_list);
		if (ret)
			return ret;
	} else {
		result->entry_a_path = empty_strslice();
		result->cmp = 1;
	}
	if (tree_id_b) {
		return manifest_tree_state_init(tree_id_b, &result->state_b, tree_list);
	} else if (result->cmp == 0) {
		result->entry_b_path = empty_strslice();
		result->cmp = -1;
		return 0;
	}
	return 1;
}

static int merge_tree_entry(struct merge_manifest_tree_state *state,
                            struct merge_name_entry *entries)
{
	if (state->cmp <= 0) {
		if (tree_entry(&state->state_a.desc, &state->entry_a)) {
			state->entry_a_path = strslice_from_str(state->entry_a.path);
		} else {
			state->entry_a_path = empty_strslice();
		}
	}
	if (state->cmp >= 0) {
		if (tree_entry(&state->state_b.desc, &state->entry_b)) {
			state->entry_b_path = strslice_from_str(state->entry_b.path);
		} else {
			state->entry_b_path = empty_strslice();
		}
	}
	if (!state->entry_a_path.len) {
		if (!state->entry_b_path.len)
			return 0;
		state->cmp = 1;
	} else if (!state->entry_b_path.len) {
		state->cmp = -1;
	} else {
		state->cmp = base_name_compare(
			state->entry_a_path.buf, state->entry_a_path.len, state->entry_a.mode,
			state->entry_b_path.buf, state->entry_b_path.len, state->entry_b.mode);
	}
	if (state->cmp <= 0) {
		entries->entry_a = &state->entry_a;
		entries->path = state->entry_a_path;
	} else {
		entries->entry_a = NULL;
	}
	if (state->cmp >= 0) {
		entries->entry_b = &state->entry_b;
		entries->path = state->entry_b_path;
	} else {
		entries->entry_b = NULL;
	}
	return 1;
}

/* Return whether two entries have matching sha1s and modes */
static int manifest_entry_equal(const struct name_entry *e1,
                                const struct name_entry *e2)
{
	return (e1->mode == e2->mode) && (oidcmp(&e1->oid, &e2->oid) == 0);
}

/* Return whether base + name matches path */
static int path_match(struct strslice base, struct strslice name,
                      struct strslice path)
{
	struct strslice slice;

	if (!strslice_startswith(path, base) ||
	    !strslice_startswith(strslice_slice(path, base.len, SIZE_MAX),
	                         name))
		return 0;

	slice = strslice_slice(path, name.len + base.len, 1);
	return slice.len == 1 && (slice.buf[0] == '\0' || slice.buf[0] == '/');
}

static void recurse_manifest(const struct object_id *ref_tree_id,
                             struct strslice ref_manifest,
                             const struct object_id *tree_id,
                             struct strbuf *manifest, struct strslice base,
                             struct object_list **tree_list)
{
	struct merge_manifest_tree_state state;
	struct merge_name_entry entries;
	struct strslice cursor;
	struct strslice underscore = { 1, "_" };
	struct strbuf dir = STRBUF_INIT;

	if (merge_manifest_tree_state_init(ref_tree_id, tree_id, &state, tree_list))
		goto corrupted;

	while (merge_tree_entry(&state, &entries)) {
		if (!strslice_startswith(entries.path, underscore))
			goto corrupted;
		cursor = ref_manifest;
		if (entries.entry_a) {
			size_t len = base.len + entries.path.len + 40;
			do {
				strslice_split_once(&ref_manifest, '\n');
			} while (S_ISDIR(entries.entry_a->mode) &&
			         (ref_manifest.len > len) &&
			         path_match(base, strslice_slice(
					entries.path, 1, SIZE_MAX), ref_manifest));
		}
		/* File/directory was removed, nothing to do */
		if (!entries.entry_b)
			continue;
		/* File/directory didn't change, copy from the reference
		 * manifest. */
		if (entries.entry_a && entries.entry_b &&
		    manifest_entry_equal(entries.entry_a, entries.entry_b)) {
			strbuf_add(manifest, cursor.buf,
			           cursor.len - ref_manifest.len);
			continue;
		}
		if (entries.entry_b && !S_ISDIR(entries.entry_b->mode)) {
			strbuf_addslice(manifest, base);
			strbuf_addslice(manifest, strslice_slice(
				entries.path, 1, SIZE_MAX));
			strbuf_addf(manifest, "%c%s%s\n", '\0',
			            oid_to_hex(&entries.entry_b->oid),
			            hgattr(entries.entry_b->mode));
			continue;
		}

		strbuf_addslice(&dir, base);
		strbuf_addslice(&dir, strslice_slice(
			entries.path, 1, SIZE_MAX));
		strbuf_addch(&dir, '/');
		if (entries.entry_a && entries.entry_b &&
                    S_ISDIR(entries.entry_a->mode)) {
			recurse_manifest(&entries.entry_a->oid, cursor,
				         &entries.entry_b->oid, manifest,
			                 strbuf_as_slice(&dir), tree_list);
		} else
			recurse_manifest(NULL, empty_strslice(),
			                 &entries.entry_b->oid, manifest,
			                 strbuf_as_slice(&dir), tree_list);
		strbuf_release(&dir);
	}

	return;
corrupted:
	die("Corrupted metadata");
}

struct manifest {
	struct object_id tree_id;
	struct strbuf content;
	struct object_list *tree_list;
};

#define MANIFEST_INIT { { { 0, } }, STRBUF_INIT, NULL }

/* For repositories with a lot of files, generating a manifest is a slow
 * operation.
 * In most cases, there are way less changes between changesets than there
 * are files in the repository, so it is much faster to generate a manifest
 * from a previously generated manifest, by applying the differences between
 * the corresponding trees.
 * Therefore, we always keep the last generated manifest.
 */
static struct manifest generated_manifest = MANIFEST_INIT;

/* The returned strbuf must not be released and/or freed. */
struct strbuf *generate_manifest(const struct object_id *oid)
{
	struct strbuf content = STRBUF_INIT;
	struct object_list *tree_list = NULL;

	/* We keep a list of all the trees we've seen while generating the
	 * previous manifest. Each tree is marked as SEEN at that time.
	 * Then, on the next manifest generation, we unmark them as SEEN,
	 * and the generation that follows will re-mark them if they are
	 * re-used. Trees that are not marked SEEN are subsequently freed.
	 */
	struct object_list *previous_list = generated_manifest.tree_list;
	while (previous_list) {
		previous_list->item->flags &= ~SEEN;
		previous_list = previous_list->next;
	}

	if (oidcmp(&generated_manifest.tree_id, oid) == 0) {
		return &generated_manifest.content;
	}

	if (generated_manifest.content.len) {
		struct strslice gm;
		gm = strbuf_slice(&generated_manifest.content, 0, SIZE_MAX);
		strbuf_grow(&content, generated_manifest.content.alloc - 1);
		recurse_manifest(&generated_manifest.tree_id, gm,
		                 oid, &content, empty_strslice(), &tree_list);
	} else {
		recurse_manifest(NULL, empty_strslice(), oid, &content,
		                 empty_strslice(), &tree_list);
	}

	oidcpy(&generated_manifest.tree_id, oid);
	strbuf_swap(&content, &generated_manifest.content);
	strbuf_release(&content);

	previous_list = generated_manifest.tree_list;
	generated_manifest.tree_list = tree_list;

	while (previous_list) {
		struct object *obj = previous_list->item;
		struct object_list *elem = previous_list;
		previous_list = elem->next;
		free(elem);
		if (!(obj->flags & SEEN))
			free_tree_buffer((struct tree *)obj);
	}
	return &generated_manifest.content;
}

static void get_manifest_oid(const struct commit *commit, struct hg_object_id *oid)
{
	const char *msg;
	const char *hex_sha1;

	msg = get_commit_buffer(commit, NULL);

	hex_sha1 = strstr(msg, "\n\n") + 2;

	if (get_sha1_hex(hex_sha1, oid->hash))
		hg_oidclr(oid);

	unuse_commit_buffer(commit, msg);
}

static void hg_sha1(struct strbuf *data, const struct hg_object_id *parent1,
                    const struct hg_object_id *parent2, struct hg_object_id *result)
{
	git_SHA_CTX ctx;

	if (!parent1)
		parent1 = &hg_null_oid;
	if (!parent2)
		parent2 = &hg_null_oid;

	git_SHA1_Init(&ctx);

	if (hg_oidcmp(parent1, parent2) < 0) {
		git_SHA1_Update(&ctx, parent1, 20);
		git_SHA1_Update(&ctx, parent2, 20);
	} else {
		git_SHA1_Update(&ctx, parent2, 20);
		git_SHA1_Update(&ctx, parent1, 20);
	}

	git_SHA1_Update(&ctx, data->buf, data->len);

	git_SHA1_Final(result->hash, &ctx);
}

int check_manifest(const struct object_id *oid,
                   struct hg_object_id *hg_oid)
{
	struct hg_object_id parent1, parent2, stored, computed;
	const struct commit *manifest_commit;
	struct strbuf *manifest;

	manifest = generate_manifest(oid);
	if (!manifest)
		return 0;

	manifest_commit = lookup_commit(the_repository, oid);
	if (!manifest_commit)
		return 0;

	if (manifest_commit->parents) {
		get_manifest_oid(manifest_commit->parents->item, &parent1);
		if (manifest_commit->parents->next) {
			get_manifest_oid(manifest_commit->parents->next->item,
			                 &parent2);
		} else
			hg_oidclr(&parent2);
	} else {
		hg_oidclr(&parent1);
		hg_oidclr(&parent2);
	}

	if (!hg_oid)
		hg_oid = &computed;

	hg_sha1(manifest, &parent1, &parent2, hg_oid);

	get_manifest_oid(manifest_commit, &stored);

	return hg_oideq(&stored, hg_oid);
}

int check_file(const struct hg_object_id *oid,
               const struct hg_object_id *parent1,
               const struct hg_object_id *parent2)
{
	struct hg_file file;
	struct hg_object_id result;

	hg_file_init(&file);
	hg_file_load(&file, oid);

	/* We do the quick and dirty thing here, for now.
	 * See details in cinnabar.githg.FileFindParents._set_parents_fallback
	 */
	hg_sha1(&file.file, parent1, parent2, &result);
	if (hg_oideq(oid, &result))
		goto ok;

	hg_sha1(&file.file, parent1, NULL, &result);
	if (hg_oideq(oid, &result))
		goto ok;

	hg_sha1(&file.file, parent2, NULL, &result);
	if (hg_oideq(oid, &result))
		goto ok;

	hg_sha1(&file.file, parent1, parent1, &result);
	if (hg_oideq(oid, &result))
		goto ok;

	hg_sha1(&file.file, NULL, NULL, &result);
	if (!hg_oideq(oid, &result))
		goto error;

ok:
	hg_file_release(&file);
	return 1;

error:
	hg_file_release(&file);
	return 0;
}

static void reset_heads(struct oid_array *heads)
{
	oid_array_clear(heads);
	// We don't want subsequent ensure_heads to refill the array,
	// so mark it as sorted, which means it's initialized.
	heads->sorted = 1;
}

void reset_manifest_heads(void)
{
	reset_heads(&manifest_heads);
}

static struct name_entry *
lazy_tree_entry_by_name(struct manifest_tree_state *state,
                        const struct object_id *tree_id,
                        const char *path)
{
	int cmp;

	if (!tree_id)
		return NULL;

	if (!state->tree) {
		if (manifest_tree_state_init(tree_id, state, NULL))
			return NULL;
	}

	while (state->desc.size &&
	       (cmp = strcmp(state->desc.entry.path, path)) < 0)
		update_tree_entry(&state->desc);

	if (state->desc.size && cmp == 0)
		return &state->desc.entry;

	return NULL;
}

struct oid_map_entry {
	struct hashmap_entry ent;
	struct object_id old_oid;
	struct object_id new_oid;
};

static int oid_map_entry_cmp(const void *cmpdata, const struct hashmap_entry *e1,
                             const struct hashmap_entry *e2, const void *keydata)
{
	const struct oid_map_entry *entry1 =
		container_of(e1, const struct oid_map_entry, ent);
	const struct oid_map_entry *entry2 =
		container_of(e2, const struct oid_map_entry, ent);

	return oidcmp(&entry1->old_oid, &entry2->old_oid);
}

static void recurse_create_git_tree(const struct object_id *tree_id,
                                    const struct object_id *reference,
                                    const struct object_id *merge_tree_id,
                                    struct object_id *result,
				    struct hashmap *cache)
{
	struct oid_map_entry k, *cache_entry = NULL;

	if (!merge_tree_id) {
		hashmap_entry_init(&k.ent, oidhash(tree_id));
		oidcpy(&k.old_oid, tree_id);
		cache_entry = hashmap_get_entry(cache, &k, ent, NULL);
	}
	if (!cache_entry) {
		struct merge_manifest_tree_state state;
		struct manifest_tree_state ref_state = { NULL, };
		struct merge_name_entry entries;
		struct strbuf tree_buf = STRBUF_INIT;

		if (merge_manifest_tree_state_init(tree_id, merge_tree_id, &state, NULL))
			goto corrupted;

		while (merge_tree_entry(&state, &entries)) {
			struct object_id oid;
			struct name_entry *entry = entries.entry_a ? entries.entry_a : entries.entry_b;
			unsigned mode = entry->mode;
			struct strslice entry_path;
			struct strslice underscore = { 1, "_" };
			if (!strslice_startswith(entries.path, underscore))
				goto corrupted;
			entry_path = strslice_slice(entries.path, 1, SIZE_MAX);
			// In some edge cases, presumably all related to the use of
			// `hg convert` before Mercurial 2.0.1, manifest trees have
			// double slashes, which end up as "_" directories in the
			// corresponding git cinnabar metadata.
			// With further changes in the subsequent Mercurial manifests,
			// those entries with double slashes are superseded with entries
			// with single slash, while still being there. So to create
			// the corresponding git commit, we need to merge both in some
			// manner.
			// Mercurial doesn't actually guarantee which of the paths would
			// actually be checked out when checking out such manifests,
			// but we always choose the single slash path. Most of the time,
			// though, both will have the same contents. At least for files.
			// Sub-directories may differ in what paths they contain, but
			// again, the files they contain are usually identical.
			if (entry_path.len == 0) {
				if (!S_ISDIR(mode))
					goto corrupted;
				if (merge_tree_id)
					continue;
				recurse_create_git_tree(
					tree_id, reference, &entry->oid, result, cache);
				goto cleanup;
			} else if (S_ISDIR(mode)) {
				struct name_entry *ref_entry;
				ref_entry = lazy_tree_entry_by_name(
					&ref_state, reference, entry_path.buf);
				recurse_create_git_tree(
					&entry->oid,
					ref_entry ? &ref_entry->oid : NULL,
					(entries.entry_b && S_ISDIR(entries.entry_b->mode))
						? &entries.entry_b->oid : NULL,
					&oid, cache);
			} else {
				const struct object_id *file_oid;
				struct hg_object_id hg_oid;
				oidcpy2hg(&hg_oid, &entry->oid);
				if (is_empty_hg_file(&hg_oid))
					file_oid = ensure_empty_blob();
				else
					file_oid = resolve_hg2git(&hg_oid, 40);
				if (!file_oid)
					goto corrupted;
				oidcpy(&oid, file_oid);
				mode &= 0777;
				if (!mode)
					mode = S_IFLNK;
				else
					mode = S_IFREG | mode;
			}
			strbuf_addf(&tree_buf, "%o ", canon_mode(mode));
			strbuf_addslice(&tree_buf, entry_path);
			strbuf_addch(&tree_buf, '\0');
			strbuf_add(&tree_buf, oid.hash, 20);
		}

		if (!merge_tree_id) {
			cache_entry = xmalloc(sizeof(k));
			cache_entry->ent = k.ent;
			cache_entry->old_oid = k.old_oid;
		}
		store_git_tree(&tree_buf, reference, cache_entry ? &cache_entry->new_oid : result);
		strbuf_release(&tree_buf);
		if (!merge_tree_id) {
			hashmap_add(cache, &cache_entry->ent);
		}

cleanup:
		if (state.state_a.tree)
			free_tree_buffer(state.state_a.tree);
		if (state.state_b.tree)
			free_tree_buffer(state.state_b.tree);
		if (ref_state.tree)
			free_tree_buffer(ref_state.tree);
	}
	if (result && cache_entry)
		oidcpy(result, &cache_entry->new_oid);
	return;

corrupted:
	die("Corrupt mercurial metadata");
}

static struct hashmap git_tree_cache;

void create_git_tree(const struct object_id *tree_id,
                     const struct object_id *ref_tree,
                     struct object_id *result)
{
	recurse_create_git_tree(tree_id, ref_tree, NULL, result, &git_tree_cache);
}

static void reset_replace_map(void)
{
	oidmap_free(the_repository->objects->replace_map, 1);
	FREE_AND_NULL(the_repository->objects->replace_map);
	the_repository->objects->replace_map_initialized = 0;
}

unsigned int replace_map_size(void)
{
	return hashmap_get_size(&the_repository->objects->replace_map->map);
}

static int count_refs(const char *refname, const struct object_id *oid,
                      int flags, void *cb_dat) {
	size_t *count = (size_t *)cb_dat;
	(*count)++;
	return 0;
}

static void init_metadata(void)
{
	struct commit *c;
	struct commit_list *cl;
	const char *msg, *body;
	struct strbuf **flags, **f;
	struct tree *tree;
	struct tree_desc desc;
	struct name_entry entry;
	struct replace_object *replace;
	size_t count = 0;

	c = lookup_commit_reference_by_name(METADATA_REF);
	if (!c) {
		oidcpy(&metadata_oid, null_oid());
		oidcpy(&changesets_oid, null_oid());
		oidcpy(&manifests_oid, null_oid());
		oidcpy(&hg2git_oid, null_oid());
		oidcpy(&git2hg_oid, null_oid());
		oidcpy(&files_meta_oid, null_oid());
		return;
	}
	oidcpy(&metadata_oid, &c->object.oid);
	cl = c->parents;
	if (!cl) die("Invalid metadata?");
	oidcpy(&changesets_oid, &cl->item->object.oid);
	cl = cl->next;
	if (!cl) die("Invalid metadata?");
	oidcpy(&manifests_oid, &cl->item->object.oid);
	cl = cl->next;
	if (!cl) die("Invalid metadata?");
	oidcpy(&hg2git_oid, &cl->item->object.oid);
	cl = cl->next;
	if (!cl) die("Invalid metadata?");
	oidcpy(&git2hg_oid, &cl->item->object.oid);
	cl = cl->next;
	if (!cl) die("Invalid metadata?");
	oidcpy(&files_meta_oid, &cl->item->object.oid);

	msg = get_commit_buffer(c, NULL);
	body = strstr(msg, "\n\n") + 2;
	flags = strbuf_split_str(body, ' ', -1);
	for (f = flags; *f; f++) {
		strbuf_trim(*f);
		if (!strcmp("files-meta", (*f)->buf))
			metadata_flags |= FILES_META;
		else if (!strcmp("unified-manifests", (*f)->buf)) {
			strbuf_list_free(flags);
			unuse_commit_buffer(c, msg);
			goto old;
		} else if (!strcmp("unified-manifests-v2", (*f)->buf))
			metadata_flags |= UNIFIED_MANIFESTS_v2;
		else {
			strbuf_list_free(flags);
			unuse_commit_buffer(c, msg);
			goto new;
		}
	}
	strbuf_list_free(flags);
	unuse_commit_buffer(c, msg);

	if (!(metadata_flags & (FILES_META | UNIFIED_MANIFESTS_v2)))
		goto old;

	for_each_ref_in("refs/cinnabar/branches/", count_refs, &count);
	if (count)
		goto old;

	reset_replace_map();
	the_repository->objects->replace_map =
		xmalloc(sizeof(*the_repository->objects->replace_map));
	oidmap_init(the_repository->objects->replace_map, 0);
	the_repository->objects->replace_map_initialized = 1;

	tree = get_commit_tree(c);
	parse_tree(tree);
	init_tree_desc(&desc, tree->buffer, tree->size);
	while (tree_entry(&desc, &entry)) {
		struct object_id original_oid;
		if (entry.pathlen != 40 ||
		    get_oid_hex(entry.path, &original_oid)) {
			struct strbuf buf = STRBUF_INIT;
			strbuf_add(&buf, entry.path, entry.pathlen);
			warning(_("bad replace name: %s"), buf.buf);
			strbuf_release(&buf);
			continue;
		}
		if (oideq(&entry.oid, &original_oid)) {
			warning(_("self-referencing graft: %s"),
				oid_to_hex(&original_oid));
			continue;
		}
		replace = xmalloc(sizeof(*replace));
		oidcpy(&replace->original.oid, &original_oid);
		oidcpy(&replace->replacement, &entry.oid);
		if (oidmap_put(the_repository->objects->replace_map, replace))
			die(_("duplicate replace: %s"),
			    oid_to_hex(&replace->original.oid));
	}
	if (the_repository->objects->replace_map->map.tablesize == 0) {
		count = 0;
		for_each_ref_in("refs/cinnabar/replace/", count_refs, &count);
		if (count > 0)
			goto old;
	}
	return;
old:
	die("Metadata from git-cinnabar versions older than 0.5.0 is not "
	    "supported.\n"
	    "Please run `git cinnabar upgrade` with version 0.5.x first.");
new:
	die("It looks like this repository was used with a newer version of "
	    "git-cinnabar. Cannot use this version.");
#if 0
upgrade:
	die("Git-cinnabar metadata needs upgrade. "
	    "Please run `git cinnabar upgrade`.");
#endif
}

void dump_ref_updates(void);

extern void reset_changeset_heads(void);

void do_reload(void)
{
	if (notes_initialized(&git2hg))
		free_notes(&git2hg);

	if (notes_initialized(&hg2git))
		free_notes(&hg2git);

	if (notes_initialized(&files_meta))
		free_notes(&files_meta);

	oidset_clear(&hg2git_seen);

	hashmap_clear_and_free(&git_tree_cache, struct oid_map_entry, ent);
	hashmap_init(&git_tree_cache, oid_map_entry_cmp, NULL, 0);

	oid_array_clear(&manifest_heads);

	dump_ref_updates();

	metadata_flags = 0;
	reset_replace_map();
	init_metadata();
	reset_changeset_heads();
}

static void init_git_config(void)
{
	struct child_process proc = CHILD_PROCESS_INIT;
	struct strbuf path = STRBUF_INIT;
	const char *env = getenv(EXEC_PATH_ENVIRONMENT);
	/* As the helper is not necessarily built with the same build options
	 * as git (because it's built separately), the way its libgit.a is
	 * going to find the system gitconfig may not match git's, and there
	 * might be important configuration items there (like http.sslcainfo
	 * on git for windows).
	 * Trick git into giving us the path to it system gitconfig. */
	if (env && *env) {
		setup_path();
	}
	strvec_pushl(&proc.args, "git", "config", "--system", "-e", NULL);
	strvec_push(&proc.env, "GIT_EDITOR=echo");
	proc.no_stdin = 1;
	proc.no_stderr = 1;
	/* We don't really care about the capture_command return value. If
	 * the path we get is empty we'll know it failed. */
	capture_command(&proc, &path, 0);
	strbuf_trim_trailing_newline(&path);

	/* If we couldn't get a path, then so be it. We may just not have
	 * a complete configuration. */
	if (path.len)
		setenv("GIT_CONFIG_SYSTEM", path.buf, 1);

	strbuf_release(&path);
}

static void cleanup_git_config(void)
{
	const char *value;
	if (!git_config_get_value("cinnabar.fsck", &value)) {
		// We used to set cinnabar.fsck globally, then locally.
		// Remove both.
		char *user_config, *xdg_config;
		git_global_config(&user_config, &xdg_config);
		if (user_config) {
			if (access_or_warn(user_config, R_OK, 0) &&
				xdg_config &&
				!access_or_warn(xdg_config, R_OK, 0))
			{
				git_config_set_in_file_gently(
					xdg_config, "cinnabar.fsck", NULL);
			} else {
				git_config_set_in_file_gently(
					user_config, "cinnabar.fsck", NULL);
			}
		}
		free(user_config);
		free(xdg_config);
		user_config = git_pathdup("config");
		if (user_config) {
			git_config_set_in_file_gently(
				user_config, "cinnabar.fsck", NULL);
		}
		free(user_config);
	}
}

static void restore_sigpipe_to_default(void)
{
	sigset_t unblock;

	sigemptyset(&unblock);
	sigaddset(&unblock, SIGPIPE);
	sigprocmask(SIG_UNBLOCK, &unblock, NULL);
	signal(SIGPIPE, SIG_DFL);
}

const char *remote_get_name(const struct remote *remote)
{
	return remote->name;
}

void remote_get_url(const struct remote *remote, const char * const **url,
                    int* url_nr)
{
	*url = remote->url;
	*url_nr = remote->url_nr;
}

int remote_skip_default_update(const struct remote *remote)
{
	return remote->skip_default_update;
}

static int nongit = 0;

extern NORETURN void do_panic(const char *err, size_t len);

static NORETURN void die_panic(const char *err, va_list params)
{
	char msg[4096];
	int len = vsnprintf(msg, sizeof(msg), err, params);
	do_panic(msg, (size_t)(len < 0) ? 0 : len);
}

void init_cinnabar(const char *argv0)
{
	set_die_routine(die_panic);

	// Initialization from common-main.c.
	sanitize_stdfds();
	restore_sigpipe_to_default();

	git_resolve_executable_dir(argv0);

	git_setup_gettext();

	initialize_the_repository();

	attr_start();

	init_git_config();
	setup_git_directory_gently(&nongit);
	git_config(git_diff_basic_config, NULL);
	cleanup_git_config();
	save_commit_buffer = 0;
	warn_on_object_refname_ambiguity = 0;
}

static int initialized = 0;

int init_cinnabar_2(void)
{
	if (nongit) {
		return 0;
	}
	init_metadata();
	hashmap_init(&git_tree_cache, oid_map_entry_cmp, NULL, 0);
	initialized = 1;
	return 1;
}

void done_cinnabar(void)
{
	if (notes_initialized(&git2hg))
		free_notes(&git2hg);

	if (notes_initialized(&hg2git))
		free_notes(&hg2git);

	if (notes_initialized(&files_meta))
		free_notes(&files_meta);

	oidset_clear(&hg2git_seen);

	hashmap_clear_and_free(&git_tree_cache, struct oid_map_entry, ent);
}

int common_exit(const char *file, int line, int code)
{
	return code;
}
