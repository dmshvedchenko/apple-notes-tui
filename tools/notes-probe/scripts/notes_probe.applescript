use framework "Foundation"
use scripting additions

-- Phase 0 capability probe for Notes.app.
-- Every operation goes through the public AppleScript dictionary exposed by Notes.
-- Arguments are supplied by osascript argv; user values are never interpolated into
-- AppleScript source.

property schemaVersion : "apple-notes-probe/v1"

on run argv
	set operationName to "unknown"
	try
		if (count of argv) is 0 then error "missing probe operation" number 64
		set operationName to item 1 of argv

		if operationName is "accounts" then
			set payload to my probeAccounts()
		else if operationName is "folders" then
			set payload to my probeFolders(my argumentAt(argv, 2))
		else if operationName is "notes" then
			set payload to my probeNotes(my argumentAt(argv, 2), my argumentAt(argv, 3), my integerArgument(argv, 4))

		else if operationName is "get-note" then
			set payload to my probeGetNote(my argumentAt(argv, 2))
		else if operationName is "preview" then
			return my probePreview(my argumentAt(argv, 2))
		else if operationName is "lookup" then
			set payload to my probeDiagnostic(my argumentAt(argv, 2), "lookup")
		else if operationName is "metadata" then
			set payload to my probeDiagnostic(my argumentAt(argv, 2), "metadata")
		else if operationName is "body" then
			set payload to my probeDiagnostic(my argumentAt(argv, 2), "body")
		else if operationName is "plaintext" then
			set payload to my probeDiagnostic(my argumentAt(argv, 2), "plaintext")
		else if operationName is "body-plaintext" then
			set payload to my probeDiagnostic(my argumentAt(argv, 2), "body-plaintext")
		else if operationName is "attachment-count" then
			set payload to my probeDiagnostic(my argumentAt(argv, 2), "attachment-count")
		else if operationName is "foundation-graph-only" then
			set payload to my foundationGraphOnly()
		else if operationName is "foundation-serialize-only" then
			set payload to my foundationSerializeOnly()
		else if operationName is "foundation-full-local" then
			set payload to my foundationFullLocal()
		else if operationName is "preview-meta-properties" then
			set payload to my probeMetadataDiagnostic(my argumentAt(argv, 2), "properties")
		else if operationName is "preview-meta-properties-shape" then
			set payload to my probePropertiesShape(my argumentAt(argv, 2))
		else if operationName is "preview-meta-properties-all" then
			set payload to my probeMetadataPropertiesAll(my argumentAt(argv, 2))
		else if operationName is "preview-properties-full" then
			set payload to my probePropertiesFull(my argumentAt(argv, 2))
		else if operationName is "lookup-contextual" then
			set payload to my contextualLookupPayload(my argumentAt(argv, 2), my argumentAt(argv, 3), my argumentAt(argv, 4))
		else if operationName is "preview-contextual" then
			return my probePreviewContextual(my argumentAt(argv, 2), my argumentAt(argv, 3), my argumentAt(argv, 4))
		else if operationName starts with "preview-meta-" then
			set payload to my probeMetadataDiagnostic(my argumentAt(argv, 2), operationName)
		else if operationName is "preview-stage-full" then
			set payload to my probeSerializedStage(my argumentAt(argv, 2), operationName)
		else if operationName starts with "preview-stage-" then
			set payload to my probeSerializedStage(my argumentAt(argv, 2), operationName)
		else if operationName is "create-note" then
			set payload to my probeCreateNote(my argumentAt(argv, 2), my argumentAt(argv, 3), my argumentAt(argv, 4))
		else if operationName is "create-folder" then
			set payload to my probeCreateFolder(my argumentAt(argv, 2), my argumentAt(argv, 3))
		else if operationName is "create-child-folder" then
			set payload to my probeCreateChildFolder(my argumentAt(argv, 2), my argumentAt(argv, 3), my argumentAt(argv, 4))
		else if operationName is "reparent-folder" then
			set payload to my probeReparentFolder(my argumentAt(argv, 2), my argumentAt(argv, 3), my argumentAt(argv, 4), my argumentAt(argv, 5))
		else if operationName is "rename-folder" then
			set payload to my probeRenameFolder(my argumentAt(argv, 2), my argumentAt(argv, 3), my argumentAt(argv, 4))
		else if operationName is "delete-folder" then
			set payload to my probeDeleteFolder(my argumentAt(argv, 2), my argumentAt(argv, 3))
		else if operationName is "update-note" then
			set payload to my probeUpdateNote(my argumentAt(argv, 2), my argumentAt(argv, 3), my argumentAt(argv, 4), my booleanArgument(argv, 5), my booleanArgument(argv, 6))
		else if operationName is "move-note" then
			set payload to my probeMoveNote(my argumentAt(argv, 2), my argumentAt(argv, 3))
		else if operationName is "delete-note" then
			set payload to my probeDeleteNote(my argumentAt(argv, 2))
		else if operationName is "attachments" then
			set payload to my probeAttachments(my argumentAt(argv, 2))
		else if operationName is "preview-attachment" then
			set payload to my probePreviewAttachment(my argumentAt(argv, 2), my argumentAt(argv, 3))
		else if operationName is "export-attachment" then
			set payload to my probeExportAttachment(my argumentAt(argv, 2), my argumentAt(argv, 3))
		else if operationName is "snapshot" then
			set payload to my probeSnapshot(my integerArgument(argv, 2))
		else
			error "unknown probe operation: " & operationName number 64
		end if

		return my successEnvelope(operationName, payload)
	on error errorMessage number errorNumber
		return my failureEnvelope(operationName, errorNumber, errorMessage)
	end try
end run

on probeAccounts()
	set resultItems to {}
	tell application "Notes"
		set defaultAccountId to id of (get default account)
		set accountRefs to every account
		repeat with accountRef in accountRefs
			set accountId to id of accountRef
			set accountName to name of accountRef
			set isUpgraded to upgraded of accountRef
			set isDefault to accountId is defaultAccountId
			set defaultFolderId to missing value
			try
				set defaultFolderId to id of (get default folder of accountRef)
			end try
			set end of resultItems to "{" & ¬
				"\"id\":" & my jsonString(accountId) & "," & ¬
				"\"name\":" & my jsonString(accountName) & "," & ¬
				"\"upgraded\":" & my jsonBoolean(isUpgraded) & "," & ¬
				"\"default\":" & my jsonBoolean(isDefault) & "," & ¬
				"\"defaultFolderId\":" & my jsonMaybeText(defaultFolderId) & "}"
		end repeat
	end tell
	return "[" & my joinText(resultItems, ",") & "]"
end probeAccounts

on probeFolders(accountFilter)
	set resultItems to {}
	if accountFilter is "" then
		tell application "Notes" to set accountRefs to every account
	else
		set accountRefs to {my findAccount(accountFilter)}
	end if

	repeat with accountRef in accountRefs
		tell application "Notes"
			set accountId to id of accountRef
			set topFolders to every folder of accountRef
		end tell
		set resultItems to resultItems & my walkFolders(topFolders, accountId, "account", accountId)
	end repeat

	return "[" & my joinText(resultItems, ",") & "]"
end probeFolders

on walkFolders(folderRefs, accountId, parentKind, parentId)
	set resultItems to {}
	repeat with folderRef in folderRefs
		tell application "Notes"
			set folderId to id of folderRef
			set folderName to name of folderRef
			set isShared to shared of folderRef
			set childFolders to every folder of folderRef
		end tell
		set end of resultItems to "{" & ¬
			"\"id\":" & my jsonString(folderId) & "," & ¬
			"\"name\":" & my jsonString(folderName) & "," & ¬
			"\"accountId\":" & my jsonString(accountId) & "," & ¬
			"\"parentKind\":" & my jsonString(parentKind) & "," & ¬
			"\"parentId\":" & my jsonString(parentId) & "," & ¬
			"\"shared\":" & my jsonBoolean(isShared) & "}"
		set resultItems to resultItems & my walkFolders(childFolders, accountId, "folder", folderId)
	end repeat
	return resultItems
end walkFolders


on probeNotes(accountFilter, folderFilter, limitCount)
	set collectionState to {seenIds:{}, serializedItems:{}, uniqueNoteCount:0, matchedAccount:false, matchedFolder:false}
	tell application "Notes" to set accountRefs to every account

	repeat with accountRef in accountRefs
		try
			tell application "Notes"
				set currentAccountId to id of accountRef
				set topFolders to every folder of accountRef
			end tell
			if accountFilter is "" or currentAccountId is accountFilter then
				set matchedAccount of collectionState to true
				set collectionState to my collectNotesFromFolders(topFolders, currentAccountId, folderFilter, limitCount, collectionState)
			end if
		end try
	end repeat

	if accountFilter is not "" and matchedAccount of collectionState is false then error "account not found: " & accountFilter number 404
	if folderFilter is not "" and matchedFolder of collectionState is false then error "folder not found: " & folderFilter number 404

	set resultItems to serializedItems of collectionState
	set totalCount to uniqueNoteCount of collectionState
	set returnedCount to count of resultItems

	return "{" & ¬
		"\"items\":[" & my joinText(resultItems, ",") & "]," & ¬
		"\"returned\":" & returnedCount & "," & ¬
		"\"total\":" & totalCount & "," & ¬
		"\"truncated\":" & my jsonBoolean(returnedCount < totalCount) & "}"
end probeNotes

on collectNotesFromFolders(folderRefs, currentAccountId, folderFilter, limitCount, collectionState)
	repeat with folderRef in folderRefs
		try
			tell application "Notes"
				set currentFolderId to id of folderRef
				set childFolders to every folder of folderRef
			end tell

			if folderFilter is "" or currentFolderId is folderFilter then
				set matchedFolder of collectionState to true
				set noteValues to missing value
				try
					tell application "Notes"
						set noteValues to get {id, name, creation date, modification date, password protected, shared} of every note of folderRef
						set noteIds to item 1 of noteValues
					end tell
				on error
					set noteValues to missing value
				end try
				if noteValues is missing value then
					try
						tell application "Notes" to set noteIds to id of every note of folderRef
						repeat with noteIndex from 1 to count of noteIds
							try
								set currentNoteId to item noteIndex of noteIds
								if currentNoteId is not in seenIds of collectionState then
									set seenIds of collectionState to (seenIds of collectionState) & {currentNoteId}
									set uniqueNoteCount of collectionState to (uniqueNoteCount of collectionState) + 1
									if limitCount is 0 or (count of serializedItems of collectionState) < limitCount then
										set end of serializedItems of collectionState to my noteMetadataForIdIndividually(currentNoteId, currentFolderId)
									end if
								end if
							end try
						end repeat
					end try
				else
					repeat with noteIndex from 1 to count of noteIds
						try
							set currentNoteId to item noteIndex of noteIds
							if currentNoteId is not in seenIds of collectionState then
								set seenIds of collectionState to (seenIds of collectionState) & {currentNoteId}
								set uniqueNoteCount of collectionState to (uniqueNoteCount of collectionState) + 1
								if limitCount is 0 or (count of serializedItems of collectionState) < limitCount then
									set end of serializedItems of collectionState to my noteMetadataFromValues(noteIndex, noteValues, currentFolderId)
								end if
							end if
						end try
					end repeat
				end if
			end if

			set collectionState to my collectNotesFromFolders(childFolders, currentAccountId, folderFilter, limitCount, collectionState)
		end try
	end repeat
	return collectionState
end collectNotesFromFolders

on probeGetNote(noteId)
	set noteContext to my findNoteContext(noteId)
	set noteRef to noteReference of noteContext
	set accountId to accountId of noteContext
	set folderId to folderId of noteContext
	tell application "Notes"
		set noteName to name of noteRef
		set noteBody to body of noteRef
		set notePlaintext to plaintext of noteRef
		set createdText to (creation date of noteRef) as text
		set modifiedText to (modification date of noteRef) as text
		set isProtected to password protected of noteRef
		set isShared to shared of noteRef
	end tell
	return "{" & ¬
		"\"id\":" & my jsonString(noteId) & "," & ¬
		"\"name\":" & my jsonString(noteName) & "," & ¬
		"\"accountId\":" & my jsonString(accountId) & "," & ¬
		"\"bodyHtml\":" & my jsonString(noteBody) & "," & ¬
		"\"plaintext\":" & my jsonString(notePlaintext) & "," & ¬
		"\"folderId\":" & my jsonString(folderId) & "," & ¬
		"\"creationDateText\":" & my jsonString(createdText) & "," & ¬
		"\"modificationDateText\":" & my jsonString(modifiedText) & "," & ¬
		"\"passwordProtected\":" & my jsonBoolean(isProtected) & "," & ¬
		"\"shared\":" & my jsonBoolean(isShared) & "}"
end probeGetNote

on probePreview(noteId)
	set noteContext to my findNoteContext(noteId)
	set noteRef to noteReference of noteContext
	set accountId to accountId of noteContext
	set folderId to folderId of noteContext
	tell application "Notes"
		set noteProperties to properties of noteRef
		set noteName to name of noteProperties
		set noteBody to body of noteProperties
		set notePlaintext to plaintext of noteProperties
		set createdText to (creation date of noteProperties) as text
		set modifiedText to (modification date of noteProperties) as text
		set isProtected to password protected of noteProperties
		set isShared to shared of noteProperties
		set attachmentRefs to every attachment of noteRef
	end tell
	set attachmentItems to {}
	repeat with attachmentRef in attachmentRefs
		tell application "Notes"
			set attachmentId to id of attachmentRef
			set attachmentName to name of attachmentRef
			set contentId to content identifier of attachmentRef
			set attachmentCreatedText to (creation date of attachmentRef) as text
			set attachmentModifiedText to (modification date of attachmentRef) as text
			set attachmentShared to shared of attachmentRef
			set attachmentUrl to missing value
			try
				set attachmentUrl to URL of attachmentRef
			end try
		end tell
		set attachmentItem to current application's NSMutableDictionary's dictionary()
		my putString(attachmentItem, "id", attachmentId)
		my putString(attachmentItem, "noteId", noteId)
		my putString(attachmentItem, "name", attachmentName)
		my putOptionalString(attachmentItem, "contentIdentifier", contentId)
		my putOptionalString(attachmentItem, "url", attachmentUrl)
		my putString(attachmentItem, "creationDateText", attachmentCreatedText)
		my putString(attachmentItem, "modificationDateText", attachmentModifiedText)
		my putBoolean(attachmentItem, "shared", attachmentShared)
		set end of attachmentItems to attachmentItem
	end repeat
	set noteItem to current application's NSMutableDictionary's dictionary()
	my putString(noteItem, "id", noteId)
	my putString(noteItem, "name", noteName)
	my putString(noteItem, "accountId", accountId)
	my putString(noteItem, "folderId", folderId)
	my putString(noteItem, "bodyHtml", noteBody)
	my putString(noteItem, "plaintext", notePlaintext)
	my putString(noteItem, "creationDateText", createdText)
	my putString(noteItem, "modificationDateText", modifiedText)
	my putBoolean(noteItem, "passwordProtected", isProtected)
	my putBoolean(noteItem, "shared", isShared)
	set dataItem to current application's NSMutableDictionary's dictionary()
	dataItem's setObject:noteItem forKey:"note"
	set attachmentArray to current application's NSMutableArray's arrayWithArray:attachmentItems
	dataItem's setObject:attachmentArray forKey:"attachments"
	set rootItem to current application's NSMutableDictionary's dictionary()
	my putString(rootItem, "schemaVersion", schemaVersion)
	my putString(rootItem, "operation", "preview")
	my putBoolean(rootItem, "ok", true)
	rootItem's setObject:dataItem forKey:"data"
	return my foundationJson(rootItem)
end probePreview

on probePreviewContextual(noteId, suppliedAccountId, suppliedFolderId)
	set noteContext to my findContextualNoteContext(noteId, suppliedAccountId, suppliedFolderId)
	set noteRef to noteReference of noteContext
	set accountId to accountId of noteContext
	set folderId to folderId of noteContext
	tell application "Notes"
		set noteProperties to properties of noteRef
		set noteName to name of noteProperties
		set noteBody to body of noteProperties
		set notePlaintext to plaintext of noteProperties
		set createdText to (creation date of noteProperties) as text
		set modifiedText to (modification date of noteProperties) as text
		set isProtected to password protected of noteProperties
		set isShared to shared of noteProperties
		set attachmentRefs to every attachment of noteRef
	end tell
	return my previewFoundationPayload(noteId, accountId, folderId, noteName, noteBody, notePlaintext, createdText, modifiedText, isProtected, isShared, attachmentRefs)
end probePreviewContextual

on previewFoundationPayload(noteId, accountId, folderId, noteName, noteBody, notePlaintext, createdText, modifiedText, isProtected, isShared, attachmentRefs)
	set attachmentItems to {}
	repeat with attachmentRef in attachmentRefs
		tell application "Notes"
			set attachmentId to id of attachmentRef
			set attachmentName to name of attachmentRef
			set contentId to content identifier of attachmentRef
			set attachmentCreatedText to (creation date of attachmentRef) as text
			set attachmentModifiedText to (modification date of attachmentRef) as text
			set attachmentShared to shared of attachmentRef
			set attachmentUrl to missing value
			try
				set attachmentUrl to URL of attachmentRef
			end try
		end tell
		set attachmentItem to current application's NSMutableDictionary's dictionary()
		my putString(attachmentItem, "id", attachmentId)
		my putString(attachmentItem, "noteId", noteId)
		my putString(attachmentItem, "name", attachmentName)
		my putOptionalString(attachmentItem, "contentIdentifier", contentId)
		my putOptionalString(attachmentItem, "url", attachmentUrl)
		my putString(attachmentItem, "creationDateText", attachmentCreatedText)
		my putString(attachmentItem, "modificationDateText", attachmentModifiedText)
		my putBoolean(attachmentItem, "shared", attachmentShared)
		set end of attachmentItems to attachmentItem
	end repeat
	set noteItem to current application's NSMutableDictionary's dictionary()
	my putString(noteItem, "id", noteId)
	my putString(noteItem, "name", noteName)
	my putString(noteItem, "accountId", accountId)
	my putString(noteItem, "folderId", folderId)
	my putString(noteItem, "bodyHtml", noteBody)
	my putString(noteItem, "plaintext", notePlaintext)
	my putString(noteItem, "creationDateText", createdText)
	my putString(noteItem, "modificationDateText", modifiedText)
	my putBoolean(noteItem, "passwordProtected", isProtected)
	my putBoolean(noteItem, "shared", isShared)
	set dataItem to current application's NSMutableDictionary's dictionary()
	dataItem's setObject:noteItem forKey:"note"
	dataItem's setObject:(current application's NSMutableArray's arrayWithArray:attachmentItems) forKey:"attachments"
	set rootItem to current application's NSMutableDictionary's dictionary()
	my putString(rootItem, "schemaVersion", schemaVersion)
	my putString(rootItem, "operation", "preview")
	my putBoolean(rootItem, "ok", true)
	rootItem's setObject:dataItem forKey:"data"
	return my foundationJson(rootItem)
end previewFoundationPayload

on contextualLookupPayload(noteId, suppliedAccountId, suppliedFolderId)
	set context to my findContextualNoteContext(noteId, suppliedAccountId, suppliedFolderId)
	set resultItem to current application's NSMutableDictionary's dictionary()
	my putString(resultItem, "noteId", noteId)
	my putString(resultItem, "accountId", accountId of context)
	my putString(resultItem, "folderId", folderId of context)
	return my foundationJson(resultItem)
end contextualLookupPayload

on putString(target, keyName, value)
	target's setObject:(current application's NSString's stringWithString:(value as text)) forKey:keyName
end putString

on putOptionalString(target, keyName, value)
	if value is missing value then
		target's setObject:(current application's NSNull's null()) forKey:keyName
	else
		my putString(target, keyName, value)
	end if
end putOptionalString

on putBoolean(target, keyName, value)
	target's setObject:(current application's NSNumber's numberWithBool:value) forKey:keyName
end putBoolean

on foundationJson(rootItem)
	set resultWithError to current application's NSJSONSerialization's dataWithJSONObject:rootItem options:0 |error|:(reference)
	set jsonData to item 1 of resultWithError
	if jsonData is missing value then error "JSON serialization failed" number 500
	set jsonText to current application's NSString's alloc()'s initWithData:jsonData encoding:(current application's NSUTF8StringEncoding)
	if jsonText is missing value then error "JSON UTF-8 conversion failed" number 500
	return jsonText as text
end foundationJson

on probeDiagnostic(noteId, modeName)
	set noteContext to my findNoteContext(noteId)
	if modeName is "lookup" then return "{\"id\":" & my jsonString(noteId) & "}"
	set noteRef to noteReference of noteContext
	tell application "Notes"
		if modeName is "metadata" then
			set ignoredName to name of noteRef
			set ignoredCreated to creation date of noteRef
			set ignoredModified to modification date of noteRef
			set ignoredProtected to password protected of noteRef
			set ignoredShared to shared of noteRef
		else if modeName is "body" then
			set ignoredBody to body of noteRef
		else if modeName is "plaintext" then
			set ignoredPlaintext to plaintext of noteRef
		else if modeName is "body-plaintext" then
			set ignoredBody to body of noteRef
			set ignoredPlaintext to plaintext of noteRef
		else if modeName is "attachment-count" then
			set ignoredCount to count of attachments of noteRef
		else if modeName is "attachments" then
			set ignoredAttachments to every attachment of noteRef
		end if
	end tell
	if modeName is "attachment-count" then return "{\"id\":" & my jsonString(noteId) & ",\"count\":" & ignoredCount & "}"
	return "{\"id\":" & my jsonString(noteId) & ",\"ok\":true}"
end probeDiagnostic

on probeMetadataDiagnostic(noteId, modeName)
	set noteContext to my findNoteContext(noteId)
	set noteRef to noteReference of noteContext
	set accountId to accountId of noteContext
	set folderId to folderId of noteContext
	set resultItem to current application's NSMutableDictionary's dictionary()
	my putString(resultItem, "id", noteId)
	my putString(resultItem, "accountId", accountId)
	my putString(resultItem, "folderId", folderId)
	tell application "Notes"
		if modeName is "preview-meta-name" or modeName is "preview-meta-all" then set noteName to name of noteRef
		if modeName is "preview-meta-created" or modeName is "preview-meta-all" then set createdText to (creation date of noteRef) as text
		if modeName is "preview-meta-modified" or modeName is "preview-meta-all" then set modifiedText to (modification date of noteRef) as text
		if modeName is "preview-meta-protection" or modeName is "preview-meta-all" then set isProtected to password protected of noteRef
		if modeName is "preview-meta-shared" or modeName is "preview-meta-all" then set isShared to shared of noteRef
		if modeName is "preview-meta-properties" then
			set noteProperties to properties of noteRef
			set noteName to name of noteProperties
			set createdText to (creation date of noteProperties) as text
			set modifiedText to (modification date of noteProperties) as text
			set isProtected to password protected of noteProperties
			set isShared to shared of noteProperties
		end if
	end tell
	if modeName is "preview-meta-name" or modeName is "preview-meta-all" or modeName is "preview-meta-properties" then my putString(resultItem, "name", noteName)
	if modeName is "preview-meta-created" or modeName is "preview-meta-all" or modeName is "preview-meta-properties" then my putString(resultItem, "creationDateText", createdText)
	if modeName is "preview-meta-modified" or modeName is "preview-meta-all" or modeName is "preview-meta-properties" then my putString(resultItem, "modificationDateText", modifiedText)
	if modeName is "preview-meta-protection" or modeName is "preview-meta-all" or modeName is "preview-meta-properties" then my putBoolean(resultItem, "passwordProtected", isProtected)
	if modeName is "preview-meta-shared" or modeName is "preview-meta-all" or modeName is "preview-meta-properties" then my putBoolean(resultItem, "shared", isShared)
	return my foundationJson(resultItem)
end probeMetadataDiagnostic

on probeMetadataPropertiesAll(noteId)
	set noteContext to my findNoteContext(noteId)
	set noteRef to noteReference of noteContext
	set accountId to accountId of noteContext
	set folderId to folderId of noteContext
	tell application "Notes"
		set noteProperties to properties of noteRef
		set localName to name of noteProperties
		set localCreated to (creation date of noteProperties) as text
		set localModified to (modification date of noteProperties) as text
		set localProtected to password protected of noteProperties
		set localShared to shared of noteProperties
	end tell
	set resultItem to current application's NSMutableDictionary's dictionary()
	my putString(resultItem, "id", noteId)
	my putString(resultItem, "accountId", accountId)
	my putString(resultItem, "folderId", folderId)
	my putString(resultItem, "name", localName)
	my putString(resultItem, "creationDateText", localCreated)
	my putString(resultItem, "modificationDateText", localModified)
	my putBoolean(resultItem, "passwordProtected", localProtected)
	my putBoolean(resultItem, "shared", localShared)
	return my foundationJson(resultItem)
end probeMetadataPropertiesAll

on probePropertiesShape(noteId)
	set noteContext to my findNoteContext(noteId)
	set noteRef to noteReference of noteContext
	tell application "Notes"
		set noteProperties to properties of noteRef
	end tell
	set resultItem to current application's NSMutableDictionary's dictionary()
	my putShape(resultItem, "id", noteProperties, "id")
	my putShape(resultItem, "name", noteProperties, "name")
	my putShape(resultItem, "creationDate", noteProperties, "creation date")
	my putShape(resultItem, "modificationDate", noteProperties, "modification date")
	my putShape(resultItem, "passwordProtected", noteProperties, "password protected")
	my putShape(resultItem, "shared", noteProperties, "shared")
	my putShape(resultItem, "body", noteProperties, "body")
	my putShape(resultItem, "plaintext", noteProperties, "plaintext")
	set attachmentsShape to current application's NSMutableDictionary's dictionary()
	my putBoolean(attachmentsShape, "present", false)
	my putString(attachmentsShape, "classification", "element-collection")
	resultItem's setObject:attachmentsShape forKey:"attachments"
	return my foundationJson(resultItem)
end probePropertiesShape

on putShape(target, keyName, recordValue, fieldName)
	set shape to current application's NSMutableDictionary's dictionary()
	try
		using terms from application "Notes"
			if fieldName is "id" then set fieldValue to id of recordValue
			if fieldName is "name" then set fieldValue to name of recordValue
			if fieldName is "creation date" then set fieldValue to creation date of recordValue
			if fieldName is "modification date" then set fieldValue to modification date of recordValue
			if fieldName is "password protected" then set fieldValue to password protected of recordValue
			if fieldName is "shared" then set fieldValue to shared of recordValue
			if fieldName is "body" then set fieldValue to body of recordValue
			if fieldName is "plaintext" then set fieldValue to plaintext of recordValue
		end using terms from
		my putBoolean(shape, "present", true)
		my putString(shape, "type", my foundationTypeOf(fieldValue))
	on error
		my putBoolean(shape, "present", false)
	end try
	target's setObject:shape forKey:keyName
end putShape

on foundationTypeOf(value)
	set valueClass to class of value
	if valueClass is boolean then return "boolean"
	if valueClass is date then return "date"
	if valueClass is integer or valueClass is real then return "number"
	return "text"
end foundationTypeOf

on probePropertiesFull(noteId)
	set noteContext to my findNoteContext(noteId)
	set noteRef to noteReference of noteContext
	set accountId to accountId of noteContext
	set folderId to folderId of noteContext
	tell application "Notes"
		set noteProperties to properties of noteRef
		set attachmentRefs to every attachment of noteRef
		set localName to name of noteProperties
		set localBody to body of noteProperties
		set localPlaintext to plaintext of noteProperties
		set localCreated to (creation date of noteProperties) as text
		set localModified to (modification date of noteProperties) as text
		set localProtected to password protected of noteProperties
		set localShared to shared of noteProperties
	end tell
	set noteItem to current application's NSMutableDictionary's dictionary()
	my putString(noteItem, "id", noteId)
	my putString(noteItem, "name", localName)
	my putString(noteItem, "accountId", accountId)
	my putString(noteItem, "folderId", folderId)
	my putString(noteItem, "bodyHtml", localBody)
	my putString(noteItem, "plaintext", localPlaintext)
	my putString(noteItem, "creationDateText", localCreated)
	my putString(noteItem, "modificationDateText", localModified)
	my putBoolean(noteItem, "passwordProtected", localProtected)
	my putBoolean(noteItem, "shared", localShared)
	set dataItem to current application's NSMutableDictionary's dictionary()
	dataItem's setObject:noteItem forKey:"note"
	dataItem's setObject:(current application's NSMutableArray's array()) forKey:"attachments"
	set rootItem to current application's NSMutableDictionary's dictionary()
	my putString(rootItem, "schemaVersion", schemaVersion)
	my putString(rootItem, "operation", "preview")
	my putBoolean(rootItem, "ok", true)
	rootItem's setObject:dataItem forKey:"data"
	return my foundationJson(rootItem)
end probePropertiesFull

on probeSerializedStage(noteId, stageName)
	set noteContext to my findNoteContext(noteId)
	set noteRef to noteReference of noteContext
	set accountId to accountId of noteContext
	set folderId to folderId of noteContext
	tell application "Notes"
		set noteName to name of noteRef
		set createdText to (creation date of noteRef) as text
		set modifiedText to (modification date of noteRef) as text
		set isProtected to password protected of noteRef
		set isShared to shared of noteRef
		if stageName is "preview-stage-body" or stageName is "preview-stage-body-plaintext" or stageName is "preview-stage-note-serialized" or stageName is "preview-stage-full" then set noteBody to body of noteRef
		if stageName is "preview-stage-plaintext" or stageName is "preview-stage-body-plaintext" or stageName is "preview-stage-note-serialized" or stageName is "preview-stage-full" then set notePlaintext to plaintext of noteRef
		if stageName is "preview-stage-attachments" or stageName is "preview-stage-full" then set attachmentRefs to every attachment of noteRef
	end tell
	set noteItem to current application's NSMutableDictionary's dictionary()
	my putString(noteItem, "id", noteId)
	my putString(noteItem, "name", noteName)
	my putString(noteItem, "accountId", accountId)
	my putString(noteItem, "folderId", folderId)
	if stageName is "preview-stage-body" or stageName is "preview-stage-body-plaintext" or stageName is "preview-stage-note-serialized" then my putString(noteItem, "bodyHtml", noteBody)
	if stageName is "preview-stage-plaintext" or stageName is "preview-stage-body-plaintext" or stageName is "preview-stage-note-serialized" then my putString(noteItem, "plaintext", notePlaintext)
	my putString(noteItem, "creationDateText", createdText)
	my putString(noteItem, "modificationDateText", modifiedText)
	my putBoolean(noteItem, "passwordProtected", isProtected)
	my putBoolean(noteItem, "shared", isShared)
	set payloadItem to noteItem
	if stageName is "preview-stage-note-serialized" then
		set wrapper to current application's NSMutableDictionary's dictionary()
		wrapper's setObject:noteItem forKey:"note"
		set payloadItem to wrapper
	else if stageName is "preview-stage-attachments" then
		set payloadItem to my foundationAttachmentPayload(noteId, attachmentRefs)
	end if
	return my foundationJson(payloadItem)
	error "unknown preview diagnostic stage" number 64
end probeSerializedStage

on foundationAttachmentPayload(noteId, attachmentRefs)
	set attachmentItems to {}
	repeat with attachmentRef in attachmentRefs
		tell application "Notes"
			set attachmentId to id of attachmentRef
			set attachmentName to name of attachmentRef
			set contentId to content identifier of attachmentRef
			set createdText to (creation date of attachmentRef) as text
			set modifiedText to (modification date of attachmentRef) as text
			set isShared to shared of attachmentRef
		end tell
		set itemDict to current application's NSMutableDictionary's dictionary()
		my putString(itemDict, "id", attachmentId)
		my putString(itemDict, "noteId", noteId)
		my putString(itemDict, "name", attachmentName)
		my putOptionalString(itemDict, "contentIdentifier", contentId)
		my putString(itemDict, "creationDateText", createdText)
		my putString(itemDict, "modificationDateText", modifiedText)
		my putBoolean(itemDict, "shared", isShared)
		set end of attachmentItems to itemDict
	end repeat
	set resultDict to current application's NSMutableDictionary's dictionary()
	resultDict's setObject:(current application's NSMutableArray's arrayWithArray:attachmentItems) forKey:"attachments"
	return resultDict
end foundationAttachmentPayload

on fixedFoundationGraph()
	set noteItem to current application's NSMutableDictionary's dictionary()
	my putString(noteItem, "id", "local-note-1")
	my putString(noteItem, "name", "Foundation benchmark")
	my putString(noteItem, "accountId", "local-account-1")
	my putString(noteItem, "folderId", "local-folder-1")
	my putString(noteItem, "bodyHtml", "<div>fixed benchmark body</div>")
	my putString(noteItem, "plaintext", "fixed benchmark body")
	my putString(noteItem, "creationDateText", "2026-01-01 00:00:00")
	my putString(noteItem, "modificationDateText", "2026-01-01 00:00:00")
	my putBoolean(noteItem, "passwordProtected", false)
	my putBoolean(noteItem, "shared", false)
	set dataItem to current application's NSMutableDictionary's dictionary()
	dataItem's setObject:noteItem forKey:"note"
	dataItem's setObject:(current application's NSMutableArray's array()) forKey:"attachments"
	set rootItem to current application's NSMutableDictionary's dictionary()
	my putString(rootItem, "schemaVersion", schemaVersion)
	my putString(rootItem, "operation", "preview")
	my putBoolean(rootItem, "ok", true)
	rootItem's setObject:dataItem forKey:"data"
	return rootItem
end fixedFoundationGraph

on foundationGraphOnly()
	set ignoredGraph to my fixedFoundationGraph()
	return "{\"graphBuilt\":true,\"attachments\":0}"
end foundationGraphOnly

on foundationSerializeOnly()
	set graphItem to my fixedFoundationGraph()
	return my foundationJson(graphItem)
end foundationSerializeOnly

on foundationFullLocal()
	set graphItem to my fixedFoundationGraph()
	return my foundationJson(graphItem)
end foundationFullLocal

on serializedAttachmentStage(noteId, attachmentRefs)
	set resultItems to {}
	repeat with attachmentRef in attachmentRefs
		tell application "Notes"
			set attachmentId to id of attachmentRef
			set attachmentName to name of attachmentRef
			set contentId to content identifier of attachmentRef
			set createdText to (creation date of attachmentRef) as text
			set modifiedText to (modification date of attachmentRef) as text
			set isShared to shared of attachmentRef
		end tell
		set end of resultItems to "{\"id\":" & my jsonString(attachmentId) & ",\"noteId\":" & my jsonString(noteId) & ",\"name\":" & my jsonString(attachmentName) & ",\"contentIdentifier\":" & my jsonMaybeText(contentId) & ",\"creationDateText\":" & my jsonString(createdText) & ",\"modificationDateText\":" & my jsonString(modifiedText) & ",\"shared\":" & my jsonBoolean(isShared) & "}"
	end repeat
	return "{\"attachments\":[" & my joinText(resultItems, ",") & "]}"
end serializedAttachmentStage

on probeCreateNote(folderId, noteName, bodyHtml)
	set folderRef to my findFolder(folderId)
	tell application "Notes"
		set noteRef to make new note at folderRef with properties {name:noteName, body:bodyHtml}
		set createdId to id of noteRef
	end tell
	return my noteMetadataFromContext(my findNoteContext(createdId))
end probeCreateNote

on probeCreateFolder(accountId, folderName)
	set accountRef to my findAccount(accountId)
	tell application "Notes"
		set folderRef to make new folder at accountRef with properties {name:folderName}
		set folderId to id of folderRef
		set createdName to name of folderRef
		set isShared to shared of folderRef
	end tell
	return "{" & ¬
		"\"id\":" & my jsonString(folderId) & "," & ¬
		"\"name\":" & my jsonString(createdName) & "," & ¬
		"\"accountId\":" & my jsonString(accountId) & "," & ¬
		"\"parentKind\":\"account\"," & ¬
		"\"parentId\":" & my jsonString(accountId) & "," & ¬
		"\"shared\":" & my jsonBoolean(isShared) & "}"
end probeCreateFolder

on probeCreateChildFolder(accountId, parentFolderId, folderName)
	set parentContext to my findFolderContextInAccount(accountId, parentFolderId)
	set parentRef to folderReference of parentContext
	tell application "Notes"
		set childRef to make new folder at parentRef with properties {name:folderName}
		set childId to id of childRef
	end tell
	return my folderMetadataFromContext(accountId, my findFolderContextInAccount(accountId, childId))
end probeCreateChildFolder

on probeReparentFolder(accountId, folderId, targetKind, targetId)
	set sourceContext to my findFolderContextInAccount(accountId, folderId)
	set sourceRef to folderReference of sourceContext
	if targetKind is "folder" then
		if targetId is folderId then error "folder cannot be its own parent" number 409
		set parentContext to my findFolderContextInAccount(accountId, targetId)
		set parentRef to folderReference of parentContext
		if my folderContainsFolder(sourceRef, targetId) then error "folder cannot be reparented below its descendant" number 409
		tell application "Notes" to move sourceRef to parentRef
	else if targetKind is "account-root" then
		if targetId is not accountId then error "root target account mismatch" number 409
		set accountRef to my findAccount(accountId)
		tell application "Notes" to move sourceRef to accountRef
	else
		error "unknown reparent target kind: " & targetKind number 400
	end if
	return my folderMetadataFromContext(accountId, my findFolderContextInAccount(accountId, folderId))
end probeReparentFolder

on probeRenameFolder(accountId, folderId, folderName)
	set folderContext to my findFolderContextInAccount(accountId, folderId)
	set folderRef to folderReference of folderContext
	tell application "Notes"
		set name of folderRef to folderName
		set renamedId to id of folderRef
		set renamedName to name of folderRef
		set isShared to shared of folderRef
	end tell
	if renamedId is not folderId then error "folder identity changed unexpectedly" number 500
	return "{" & ¬
		"\"id\":" & my jsonString(renamedId) & "," & ¬
		"\"name\":" & my jsonString(renamedName) & "," & ¬
		"\"accountId\":" & my jsonString(accountId) & "," & ¬
		"\"parentKind\":" & my jsonString(parentKind of folderContext) & "," & ¬
		"\"parentId\":" & my jsonString(parentId of folderContext) & "," & ¬
		"\"shared\":" & my jsonBoolean(isShared) & "}"
end probeRenameFolder

on folderMetadataFromContext(accountId, folderContext)
	set folderRef to folderReference of folderContext
	tell application "Notes"
		set folderId to id of folderRef
		set folderName to name of folderRef
		set isShared to shared of folderRef
	end tell
	return "{" & ¬
		"\"id\":" & my jsonString(folderId) & "," & ¬
		"\"name\":" & my jsonString(folderName) & "," & ¬
		"\"accountId\":" & my jsonString(accountId) & "," & ¬
		"\"parentKind\":" & my jsonString(parentKind of folderContext) & "," & ¬
		"\"parentId\":" & my jsonString(parentId of folderContext) & "," & ¬
		"\"shared\":" & my jsonBoolean(isShared) & "}"
end folderMetadataFromContext

on probeDeleteFolder(accountId, folderId)
	set folderContext to my findFolderContextInAccount(accountId, folderId)
	set folderRef to folderReference of folderContext
	tell application "Notes"
		set noteCount to count of every note of folderRef
		set childFolderCount to count of every folder of folderRef
		if noteCount is not 0 or childFolderCount is not 0 then error "folder deletion requires an empty folder with no child folders" number 409
		set deletedId to id of folderRef
		delete folderRef
	end tell
	if deletedId is not folderId then error "folder identity changed unexpectedly" number 500
	return "{" & ¬
		"\"id\":" & my jsonString(deletedId) & "," & ¬
		"\"accountId\":" & my jsonString(accountId) & "}"
end probeDeleteFolder

on probeUpdateNote(noteId, noteName, bodyHtml, changesName, changesBody)
	set noteContext to my findNoteContext(noteId)
	set noteRef to noteReference of noteContext
	tell application "Notes"
		if changesBody then set body of noteRef to bodyHtml
		if changesName then set name of noteRef to noteName
	end tell
	return my noteMetadataFromContext(my findNoteContext(noteId))
end probeUpdateNote

on probeMoveNote(noteId, destinationFolderId)
	set noteContext to my findNoteContext(noteId)
	set noteRef to noteReference of noteContext
	set folderRef to my findFolder(destinationFolderId)
	tell application "Notes" to move noteRef to folderRef
	return my noteMetadataFromContext(my findNoteContext(noteId))
end probeMoveNote

on probeDeleteNote(noteId)
	set noteContext to my findNoteContext(noteId)
	set noteRef to noteReference of noteContext
	set sourceFolderId to folderId of noteContext
	tell application "Notes"
		set noteName to name of noteRef
		delete noteRef
	end tell
	return "{" & ¬
		"\"id\":" & my jsonString(noteId) & "," & ¬
		"\"name\":" & my jsonString(noteName) & "," & ¬
		"\"sourceFolderId\":" & my jsonString(sourceFolderId) & "," & ¬
		"\"deleted\":true," & ¬
		"\"disposition\":\"Notes.app controls recovery and permanent deletion behavior\"}"
end probeDeleteNote

on probeAttachments(noteId)
	set noteRef to my findNote(noteId)
	tell application "Notes" to set attachmentRefs to every attachment of noteRef
	set resultItems to {}
	repeat with attachmentRef in attachmentRefs
		tell application "Notes"
			set attachmentId to id of attachmentRef
			set attachmentName to name of attachmentRef
			set contentId to content identifier of attachmentRef
			set createdText to (creation date of attachmentRef) as text
			set modifiedText to (modification date of attachmentRef) as text
			set isShared to shared of attachmentRef
			set attachmentUrl to missing value
			try
				set attachmentUrl to URL of attachmentRef
			end try
		end tell
		set end of resultItems to "{" & ¬
			"\"id\":" & my jsonString(attachmentId) & "," & ¬
			"\"noteId\":" & my jsonString(noteId) & "," & ¬
			"\"name\":" & my jsonString(attachmentName) & "," & ¬
			"\"contentIdentifier\":" & my jsonMaybeText(contentId) & "," & ¬
			"\"url\":" & my jsonMaybeText(attachmentUrl) & "," & ¬
			"\"creationDateText\":" & my jsonString(createdText) & "," & ¬
			"\"modificationDateText\":" & my jsonString(modifiedText) & "," & ¬
			"\"shared\":" & my jsonBoolean(isShared) & "}"
	end repeat
	return "[" & my joinText(resultItems, ",") & "]"
end probeAttachments

on probePreviewAttachment(noteId, attachmentId)
	set attachmentRef to my findAttachment(noteId, attachmentId)
	tell application "Notes" to show attachmentRef
	return "{" & ¬
		"\"id\":" & my jsonString(attachmentId) & "," & ¬
		"\"noteId\":" & my jsonString(noteId) & "," & ¬
		"\"method\":\"Notes.app show\"}"
end probePreviewAttachment

on probeExportAttachment(noteId, attachmentId)
	set attachmentRef to my findAttachment(noteId, attachmentId)
	tell application "Notes" to set attachmentName to name of attachmentRef
	set destinationFile to choose file name with prompt "Export a read-only copy of the Notes attachment:" default name attachmentName
	tell application "Notes" to save attachmentRef in destinationFile
	set destinationPath to POSIX path of destinationFile
	return "{" & ¬
		"\"id\":" & my jsonString(attachmentId) & "," & ¬
		"\"noteId\":" & my jsonString(noteId) & "," & ¬
		"\"destination\":" & my jsonString(destinationPath) & "}"
end probeExportAttachment

on probeSnapshot(limitCount)
	set snapshotState to {accountItems:{}, folderItems:{}, seenIds:{}, noteItems:{}, uniqueNoteCount:0}
	tell application "Notes"
		set defaultAccountId to missing value
		try
			set defaultAccountId to id of (get default account)
		end try
		set accountRefs to every account
	end tell

	repeat with accountRef in accountRefs
		try
			tell application "Notes"
				set accountValues to {id, name, upgraded} of accountRef
				set currentAccountId to item 1 of accountValues
				set currentAccountName to item 2 of accountValues
				set isUpgraded to item 3 of accountValues
				set defaultFolderId to missing value
				try
					set defaultFolderId to id of (get default folder of accountRef)
				end try
				set topFolders to every folder of accountRef
			end tell
			set end of accountItems of snapshotState to "{" & ¬
				"\"id\":" & my jsonString(currentAccountId) & "," & ¬
				"\"name\":" & my jsonString(currentAccountName) & "," & ¬
				"\"upgraded\":" & my jsonBoolean(isUpgraded) & "," & ¬
				"\"default\":" & my jsonBoolean(currentAccountId is defaultAccountId) & "," & ¬
				"\"defaultFolderId\":" & my jsonMaybeText(defaultFolderId) & "}"
			set snapshotState to my collectSnapshotFolders(topFolders, currentAccountId, "account", currentAccountId, limitCount, snapshotState)
		end try
	end repeat

	set accountPayload to "[" & my joinText(accountItems of snapshotState, ",") & "]"
	set folderPayload to "[" & my joinText(folderItems of snapshotState, ",") & "]"
	set noteItemsPayload to noteItems of snapshotState
	set returnedCount to count of noteItemsPayload
	set totalCount to uniqueNoteCount of snapshotState
	set notePayload to "{" & ¬
		"\"items\":[" & my joinText(noteItemsPayload, ",") & "]," & ¬
		"\"returned\":" & returnedCount & "," & ¬
		"\"total\":" & totalCount & "," & ¬
		"\"truncated\":" & my jsonBoolean(returnedCount < totalCount) & "}"
	return "{" & ¬
		"\"accounts\":" & accountPayload & "," & ¬
		"\"folders\":" & folderPayload & "," & ¬
		"\"notes\":" & notePayload & "}"
end probeSnapshot

on collectSnapshotFolders(folderRefs, currentAccountId, parentKind, parentId, limitCount, snapshotState)
	repeat with folderRef in folderRefs
		try
			tell application "Notes"
				set folderValues to {id, name, shared} of folderRef
				set currentFolderId to item 1 of folderValues
				set currentFolderName to item 2 of folderValues
				set isShared to item 3 of folderValues
				set childFolders to every folder of folderRef
			end tell
			set end of folderItems of snapshotState to "{" & ¬
				"\"id\":" & my jsonString(currentFolderId) & "," & ¬
				"\"name\":" & my jsonString(currentFolderName) & "," & ¬
				"\"accountId\":" & my jsonString(currentAccountId) & "," & ¬
				"\"parentKind\":" & my jsonString(parentKind) & "," & ¬
				"\"parentId\":" & my jsonString(parentId) & "," & ¬
				"\"shared\":" & my jsonBoolean(isShared) & "}"

			set noteValues to missing value
			try
					tell application "Notes"
					set noteValues to get {id, name, creation date, modification date, password protected, shared} of every note of folderRef
						set noteIds to item 1 of noteValues
					end tell
			on error
				set noteValues to missing value
			end try
			if noteValues is missing value then
				try
					tell application "Notes" to set noteIds to id of every note of folderRef
					repeat with noteIndex from 1 to count of noteIds
						try
							set currentNoteId to item noteIndex of noteIds
							if currentNoteId is not in seenIds of snapshotState then
								set seenIds of snapshotState to (seenIds of snapshotState) & {currentNoteId}
								set uniqueNoteCount of snapshotState to (uniqueNoteCount of snapshotState) + 1
								if limitCount is 0 or (count of noteItems of snapshotState) < limitCount then
									set end of noteItems of snapshotState to my noteMetadataForIdIndividually(currentNoteId, currentFolderId)
								end if
							end if
						end try
					end repeat
				end try
			else
				repeat with noteIndex from 1 to count of noteIds
					try
						set currentNoteId to item noteIndex of noteIds
						if currentNoteId is not in seenIds of snapshotState then
							set seenIds of snapshotState to (seenIds of snapshotState) & {currentNoteId}
							set uniqueNoteCount of snapshotState to (uniqueNoteCount of snapshotState) + 1
							if limitCount is 0 or (count of noteItems of snapshotState) < limitCount then
								set end of noteItems of snapshotState to my noteMetadataFromValues(noteIndex, noteValues, currentFolderId)
							end if
						end if
					end try
				end repeat
			end if

			set snapshotState to my collectSnapshotFolders(childFolders, currentAccountId, "folder", currentFolderId, limitCount, snapshotState)
		end try
	end repeat
	return snapshotState
end collectSnapshotFolders


on noteMetadata(noteRef)
	tell application "Notes" to set noteId to id of noteRef
	return my noteMetadataFromContext(my findNoteContext(noteId))
end noteMetadata

on noteMetadataFromContext(noteContext)
	set noteRef to noteReference of noteContext
	set noteId to noteId of noteContext
	set folderId to folderId of noteContext
	tell application "Notes"
		set noteName to name of noteRef
		set createdText to (creation date of noteRef) as text
		set modifiedText to (modification date of noteRef) as text
		set isProtected to password protected of noteRef
		set isShared to shared of noteRef
		set attachmentCount to missing value
		try
			set attachmentCount to count of every attachment of noteRef
		end try
	end tell
	return "{" & ¬
		"\"id\":" & my jsonString(noteId) & "," & ¬
		"\"name\":" & my jsonString(noteName) & "," & ¬
		"\"folderId\":" & my jsonString(folderId) & "," & ¬
		"\"creationDateText\":" & my jsonString(createdText) & "," & ¬
		"\"modificationDateText\":" & my jsonString(modifiedText) & "," & ¬
		"\"passwordProtected\":" & my jsonBoolean(isProtected) & "," & ¬
		"\"shared\":" & my jsonBoolean(isShared) & "," & ¬
		"\"attachmentCount\":" & my jsonInteger(attachmentCount) & "}"
end noteMetadataFromContext

on noteMetadataFromValues(noteIndex, noteValues, currentFolderId)
	set noteId to item noteIndex of item 1 of noteValues
	set noteName to item noteIndex of item 2 of noteValues
	set createdText to (item noteIndex of item 3 of noteValues) as text
	set modifiedText to (item noteIndex of item 4 of noteValues) as text
	set isProtected to item noteIndex of item 5 of noteValues
	set isShared to item noteIndex of item 6 of noteValues
	return "{" & ¬
		"\"id\":" & my jsonString(noteId) & "," & ¬
		"\"name\":" & my jsonString(noteName) & "," & ¬
		"\"folderId\":" & my jsonString(currentFolderId) & "," & ¬
		"\"creationDateText\":" & my jsonString(createdText) & "," & ¬
		"\"modificationDateText\":" & my jsonString(modifiedText) & "," & ¬
		"\"passwordProtected\":" & my jsonBoolean(isProtected) & "," & ¬
		"\"shared\":" & my jsonBoolean(isShared) & "}"
end noteMetadataFromValues

on noteMetadataForIdIndividually(noteId, currentFolderId)
	tell application "Notes"
		set currentNote to note id noteId
		set noteName to name of currentNote
		set createdText to (creation date of currentNote) as text
		set modifiedText to (modification date of currentNote) as text
		set isProtected to password protected of currentNote
		set isShared to shared of currentNote
	end tell
	return "{" & ¬
		"\"id\":" & my jsonString(noteId) & "," & ¬
		"\"name\":" & my jsonString(noteName) & "," & ¬
		"\"folderId\":" & my jsonString(currentFolderId) & "," & ¬
		"\"creationDateText\":" & my jsonString(createdText) & "," & ¬
		"\"modificationDateText\":" & my jsonString(modifiedText) & "," & ¬
		"\"passwordProtected\":" & my jsonBoolean(isProtected) & "," & ¬
		"\"shared\":" & my jsonBoolean(isShared) & "}"
end noteMetadataForIdIndividually

on findAccount(accountId)
	tell application "Notes"
		set matches to every account whose id is accountId
		if (count of matches) is 0 then error "account not found: " & accountId number 404
		return item 1 of matches
	end tell
end findAccount

on findFolder(folderId)
	tell application "Notes"
		set matches to every folder whose id is folderId
		if (count of matches) is 0 then error "folder not found: " & folderId number 404
		return item 1 of matches
	end tell
end findFolder

on findFolderContextInAccount(accountId, folderId)
	set accountRef to my findAccount(accountId)
	tell application "Notes" to set folderRefs to every folder of accountRef
	set folderContext to my findFolderContextInFolders(folderRefs, folderId, "account", accountId)
	if folderContext is missing value then error "folder not found in account: " & folderId number 404
	return folderContext
end findFolderContextInAccount

on findFolderContextInFolders(folderRefs, folderId, parentKind, parentId)
	repeat with folderRef in folderRefs
		try
			tell application "Notes"
				set currentFolderId to id of folderRef
				set childFolders to every folder of folderRef
			end tell
			if currentFolderId is folderId then return {folderReference:folderRef, parentKind:parentKind, parentId:parentId}
			set nestedFolderContext to my findFolderContextInFolders(childFolders, folderId, "folder", currentFolderId)
			if nestedFolderContext is not missing value then return nestedFolderContext
		end try
	end repeat
	return missing value
end findFolderContextInFolders

on folderContainsFolder(folderRef, targetFolderId)
	tell application "Notes" to set childFolders to every folder of folderRef
	repeat with childRef in childFolders
		tell application "Notes" to set childId to id of childRef
		if childId is targetFolderId then return true
		if my folderContainsFolder(childRef, targetFolderId) then return true
	end repeat
	return false
end folderContainsFolder

on findAttachment(noteId, attachmentId)
	set noteRef to my findNote(noteId)
	tell application "Notes" to set attachmentRefs to every attachment of noteRef
	repeat with attachmentRef in attachmentRefs
		try
			tell application "Notes" to set currentId to id of attachmentRef
			if currentId is attachmentId then return attachmentRef
		end try
	end repeat
	error "attachment not found: " & attachmentId number 404
end findAttachment

on findNote(noteId)
	tell application "Notes"
		set matches to every note whose id is noteId
		if (count of matches) is 0 then error "note not found: " & noteId number 404
		return item 1 of matches
	end tell
end findNote

on findNoteContext(targetNoteId)
	tell application "Notes" to set accountRefs to every account
	repeat with accountRef in accountRefs
		try
			tell application "Notes"
				set currentAccountId to id of accountRef
				set folderRefs to every folder of accountRef
			end tell
			set noteContext to my findNoteContextInFolders(folderRefs, currentAccountId, targetNoteId)
			if noteContext is not missing value then return noteContext
		end try
	end repeat
	error "note not found: " & targetNoteId number 404
end findNoteContext

on findContextualNoteContext(targetNoteId, targetAccountId, targetFolderId)
	tell application "Notes" to set accountRefs to every account
	repeat with accountRef in accountRefs
		try
			tell application "Notes" to set currentAccountId to id of accountRef
			if currentAccountId is targetAccountId then
				tell application "Notes" to set folderRefs to every folder of accountRef
				set folderRef to my findFolderById(folderRefs, targetFolderId)
				if folderRef is not missing value then
					tell application "Notes"
						set noteRefs to every note of folderRef
						set noteIds to id of every note of folderRef
					end tell
					repeat with noteIndex from 1 to count of noteIds
						if item noteIndex of noteIds is targetNoteId then
							return {noteReference:(item noteIndex of noteRefs), noteId:targetNoteId, accountId:targetAccountId, folderId:targetFolderId}
						end if
					end repeat
				end if
			end if
		end try
	end repeat
	error "context mismatch: note not found in supplied account/folder" number 404
end findContextualNoteContext

on findFolderById(folderRefs, targetFolderId)
	repeat with folderRef in folderRefs
		try
			tell application "Notes"
			set currentFolderId to id of folderRef
			set childFolders to every folder of folderRef
		end tell
			if currentFolderId is targetFolderId then return folderRef
			set nestedMatch to my findFolderById(childFolders, targetFolderId)
			if nestedMatch is not missing value then return nestedMatch
		end try
	end repeat
	return missing value
end findFolderById

on findNoteContextInFolders(folderRefs, currentAccountId, targetNoteId)
	repeat with folderRef in folderRefs
		try
			tell application "Notes"
				set currentFolderId to id of folderRef
				set noteRefs to every note of folderRef
				set noteIds to id of every note of folderRef
				set childFolders to every folder of folderRef
			end tell
			repeat with noteIndex from 1 to (count of noteIds)
				if (item noteIndex of noteIds) is targetNoteId then
					return {noteReference:(item noteIndex of noteRefs), noteId:targetNoteId, accountId:currentAccountId, folderId:currentFolderId}
				end if
			end repeat
			set noteContext to my findNoteContextInFolders(childFolders, currentAccountId, targetNoteId)
			if noteContext is not missing value then return noteContext
		end try
	end repeat
	return missing value
end findNoteContextInFolders

on argumentAt(argv, itemIndex)
	if (count of argv) < itemIndex then error "missing internal argument " & itemIndex number 64
	return item itemIndex of argv
end argumentAt

on integerArgument(argv, itemIndex)
	set rawValue to my argumentAt(argv, itemIndex)
	try
		return rawValue as integer
	on error
		error "argument " & itemIndex & " is not an integer" number 64
	end try
end integerArgument

on booleanArgument(argv, itemIndex)
	set rawValue to my argumentAt(argv, itemIndex)
	if rawValue is "true" then return true
	if rawValue is "false" then return false
	error "argument " & itemIndex & " is not a boolean" number 64
end booleanArgument

on successEnvelope(operationName, payload)
	return "{" & ¬
		"\"schemaVersion\":" & my jsonString(schemaVersion) & "," & ¬
		"\"operation\":" & my jsonString(operationName) & "," & ¬
		"\"ok\":true," & ¬
		"\"data\":" & payload & "}"
end successEnvelope

on failureEnvelope(operationName, errorNumber, errorMessage)
	return "{" & ¬
		"\"schemaVersion\":" & my jsonString(schemaVersion) & "," & ¬
		"\"operation\":" & my jsonString(operationName) & "," & ¬
		"\"ok\":false," & ¬
		"\"error\":{" & ¬
		"\"source\":\"AppleScript\"," & ¬
		"\"code\":" & my jsonString(errorNumber as text) & "," & ¬
		"\"message\":" & my jsonString(errorMessage) & "}}"
end failureEnvelope

on jsonBoolean(value)
	if value then return "true"
	return "false"
end jsonBoolean

on jsonMaybeText(value)
	if value is missing value then return "null"
	return my jsonString(value as text)
end jsonMaybeText

on jsonMaybeInteger(value)
	if value is missing value then return "null"
	return value as text
end jsonMaybeInteger

on jsonInteger(value)
	try
		set integerValue to value as integer
		if integerValue < 0 then error "integer must be non-negative"
		return integerValue as text
	on error
		error "attachment count is not a non-negative integer" number 500
	end try
end jsonInteger

on jsonString(value)
	set nsValue to current application's NSString's stringWithString:(value as text)
	set resultWithError to current application's NSJSONSerialization's dataWithJSONObject:nsValue options:(current application's NSJSONWritingFragmentsAllowed) |error|:(reference)
	set jsonData to item 1 of resultWithError
	set serializationError to item 2 of resultWithError
	if jsonData is missing value then error "JSON serialization failed" number 500
	set jsonText to current application's NSString's alloc()'s initWithData:jsonData encoding:(current application's NSUTF8StringEncoding)
	if jsonText is missing value then error "JSON UTF-8 conversion failed" number 500
	return jsonText as text
end jsonString

on hexDigit(value)
	return character (value + 1) of "0123456789abcdef"
end hexDigit

on replaceText(searchText, replacementText, sourceText)
	set previousDelimiters to AppleScript's text item delimiters
	set AppleScript's text item delimiters to searchText
	set textParts to text items of sourceText
	set AppleScript's text item delimiters to replacementText
	set joinedText to textParts as text
	set AppleScript's text item delimiters to previousDelimiters
	return joinedText
end replaceText

on joinText(textItems, separatorText)
	if (count of textItems) is 0 then return ""
	set previousDelimiters to AppleScript's text item delimiters
	set AppleScript's text item delimiters to separatorText
	set joinedText to textItems as text
	set AppleScript's text item delimiters to previousDelimiters
	return joinedText
end joinText
