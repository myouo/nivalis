//! Bounded in-memory repository used by the interactive prototype and tests.

use crate::{
    AccountItem, MailDetail, MailSummary,
    ui_identity::{AccountKey, EntityKey},
};
use slint::{Color, SharedString};

const ALL_ACCOUNTS: i32 = 0;
const WORK_ACCOUNT: i32 = 1;
const PERSONAL_ACCOUNT: i32 = 2;
const READING_ACCOUNT: i32 = 3;
const PAGE_SIZE: usize = 50;
#[cfg(test)]
const PREVIEW_MAX_CHARS: usize = 280;
const BODY_PREVIEW_MAX_CHARS: usize = 16_384;
const ACCOUNT_MODEL_ROWS: usize = ACCOUNT_PROFILES.len() + 1;

#[derive(Clone, Copy)]
struct AccountProfile {
    id: i32,
    name: &'static str,
    address: &'static str,
    initials: &'static str,
    status: &'static str,
    accent: (u8, u8, u8),
    has_error: bool,
}

const ACCOUNT_PROFILES: [AccountProfile; 3] = [
    AccountProfile {
        id: WORK_ACCOUNT,
        name: "Work",
        address: "you@northstar.studio",
        initials: "NW",
        status: "Local sample",
        accent: (50, 96, 78),
        has_error: false,
    },
    AccountProfile {
        id: PERSONAL_ACCOUNT,
        name: "Personal",
        address: "hello@nivalis.local",
        initials: "NP",
        status: "Sample sign-in issue",
        accent: (145, 78, 82),
        has_error: true,
    },
    AccountProfile {
        id: READING_ACCOUNT,
        name: "Reading",
        address: "reader@nivalis.local",
        initials: "NR",
        status: "Local sample",
        accent: (174, 99, 42),
        has_error: false,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Folder {
    Inbox,
    Starred,
    Unread,
    Archive,
    Sent,
    Drafts,
    Trash,
}

impl Folder {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "Inbox" => Some(Self::Inbox),
            "Starred" => Some(Self::Starred),
            "Unread" => Some(Self::Unread),
            "Archive" => Some(Self::Archive),
            "Sent" => Some(Self::Sent),
            "Drafts" => Some(Self::Drafts),
            "Trash" => Some(Self::Trash),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Inbox => "Inbox",
            Self::Starred => "Starred",
            Self::Unread => "Unread",
            Self::Archive => "Archive",
            Self::Sent => "Sent",
            Self::Drafts => "Drafts",
            Self::Trash => "Trash",
        }
    }
}

#[derive(Debug)]
struct DeletedMail {
    id: i32,
    previous_folder: Folder,
}

#[derive(Clone, Debug)]
pub struct MailRecord {
    pub id: i32,
    pub account_id: i32,
    pub sender: SharedString,
    pub email: SharedString,
    pub initials: SharedString,
    pub subject: SharedString,
    pub preview: SharedString,
    pub body: SharedString,
    pub time: SharedString,
    pub date: SharedString,
    folder: Folder,
    pub unread: bool,
    pub starred: bool,
    pub has_attachment: bool,
    pub accent: (u8, u8, u8),
}

impl MailRecord {
    fn to_summary(&self, account_label: SharedString) -> MailSummary {
        MailSummary {
            id: encode_entity_id(self.id),
            account_id: encode_account_id(self.account_id),
            account_label,
            sender: self.sender.clone(),
            initials: self.initials.clone(),
            subject: self.subject.clone(),
            preview: self.preview.clone(),
            time: self.time.clone(),
            unread: self.unread,
            starred: self.starred,
            has_attachment: self.has_attachment,
            avatar_color: Color::from_rgb_u8(self.accent.0, self.accent.1, self.accent.2),
        }
    }

    fn to_detail(&self) -> MailDetail {
        let (body, body_truncated) = bounded_text(&self.body, BODY_PREVIEW_MAX_CHARS);
        MailDetail {
            id: encode_entity_id(self.id),
            sender: self.sender.clone(),
            email: self.email.clone(),
            initials: self.initials.clone(),
            subject: self.subject.clone(),
            body,
            body_truncated,
            date: self.date.clone(),
            folder: self.folder.name().into(),
            starred: self.starred,
            has_attachment: self.has_attachment,
            avatar_color: Color::from_rgb_u8(self.accent.0, self.accent.1, self.accent.2),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MailStats {
    pub message_total: i32,
    pub inbox_count: i32,
    pub starred_count: i32,
    pub draft_count: i32,
    pub account_unread: [i32; ACCOUNT_MODEL_ROWS],
}

pub struct MailView {
    pub rows: Vec<MailSummary>,
    pub stats: MailStats,
}

#[derive(Debug)]
pub struct MailStore {
    messages: Vec<MailRecord>,
    active_account_id: i32,
    active_folder: Folder,
    query: String,
    selected_id: Option<i32>,
    #[cfg(test)]
    next_id: i32,
    last_deleted: Option<DeletedMail>,
    account_labels: [SharedString; ACCOUNT_MODEL_ROWS],
}

impl MailStore {
    pub fn demo() -> Self {
        let messages = vec![
            MailRecord {
                id: 1,
                account_id: WORK_ACCOUNT,
                sender: "Maya Chen".into(),
                email: "maya.chen@northwind.com".into(),
                initials: "MC".into(),
                subject: "Quarterly planning notes".into(),
                preview: "Please find my notes from our planning session and the next steps...".into(),
                body: "Hi team,\n\nPlease find my notes from our quarterly planning session. I highlighted the key priorities and next steps we discussed.\n\nKey takeaways:\n\n\u{2022} Focus on customer onboarding improvements\n\u{2022} Ship the analytics dashboard in Q3\n\u{2022} Expand integrations based on top user requests\n\u{2022} Revisit pricing experiment results next month\n\nLet me know if you have any questions or want to sync further.\n\nBest,\nMaya".into(),
                time: "09:41".into(),
                date: "Today, 09:41".into(),
                folder: Folder::Inbox,
                unread: true,
                starred: true,
                has_attachment: true,
                accent: (88, 148, 222),
            },
            MailRecord {
                id: 2,
                account_id: WORK_ACCOUNT,
                sender: "Linus Berg".into(),
                email: "linus@fjord.systems".into(),
                initials: "LB".into(),
                subject: "Notes from the architecture review".into(),
                preview: "The team agreed on the event boundary. Here are the three follow-ups...".into(),
                body: "Hello,\n\nThe team agreed on the event boundary and the migration sequence. We can keep the first release deliberately small: local cache, deterministic sync, and a single source of truth for message state.\n\nThree follow-ups:\n\n1. Document retry behavior.\n2. Add metrics around sync latency.\n3. Run the offline scenario before beta.\n\nRegards,\nLinus".into(),
                time: "08:17".into(),
                date: "Today, 08:17".into(),
                folder: Folder::Inbox,
                unread: true,
                starred: false,
                has_attachment: false,
                accent: (42, 93, 128),
            },
            MailRecord {
                id: 3,
                account_id: READING_ACCOUNT,
                sender: "Field Notes".into(),
                email: "dispatch@fieldnotes.news".into(),
                initials: "FN".into(),
                subject: "Saturday reading: useful constraints".into(),
                preview: "Five essays on why the right limitation can improve creative work...".into(),
                body: "This weekend's collection looks at useful constraints: how small teams make faster decisions, why fewer tools can lead to better systems, and what product makers can learn from book editors.\n\nYour reading list is saved and ready whenever you have a quiet hour.".into(),
                time: "Sat".into(),
                date: "Saturday, 11:30".into(),
                folder: Folder::Inbox,
                unread: false,
                starred: true,
                has_attachment: false,
                accent: (183, 104, 43),
            },
            MailRecord {
                id: 4,
                account_id: PERSONAL_ACCOUNT,
                sender: "Sofia Rossi".into(),
                email: "sofia@atelier-rossi.it".into(),
                initials: "SR".into(),
                subject: "Re: Helsinki in September".into(),
                preview: "The smaller place near the harbor is available. I put the details below...".into(),
                body: "Good news,\n\nThe smaller place near the harbor is available for our dates. It is a ten-minute walk from the studio and the morning ferry stops on the same street. I put the full details and cancellation terms in the attached PDF.\n\nLet me know before Friday and I will reserve it.\n\nSofia".into(),
                time: "Fri".into(),
                date: "Friday, 16:08".into(),
                folder: Folder::Inbox,
                unread: false,
                starred: false,
                has_attachment: true,
                accent: (151, 73, 76),
            },
            MailRecord {
                id: 5,
                account_id: WORK_ACCOUNT,
                sender: "Nivalis Team".into(),
                email: "team@nivalis.local".into(),
                initials: "NT".into(),
                subject: "A calmer way to work through email".into(),
                preview: "Welcome to Nivalis. This inbox is designed around focus, not volume...".into(),
                body: "Welcome to Nivalis.\n\nThis inbox is designed around focus, not volume. Search is immediate, reading stays uncluttered, and every action is reversible while the app is running.\n\nThis preview uses local data only. Connectors for IMAP, SMTP, secure credentials, and durable storage can be added behind the same UI model.\n\nEnjoy the quiet.\nThe Nivalis team".into(),
                time: "Thu".into(),
                date: "Thursday, 10:00".into(),
                folder: Folder::Inbox,
                unread: false,
                starred: false,
                has_attachment: false,
                accent: (104, 113, 107),
            },
            MailRecord {
                id: 8,
                account_id: WORK_ACCOUNT,
                sender: "Daniel Wright".into(),
                email: "daniel@northstar.studio".into(),
                initials: "DW".into(),
                subject: "Re: Product roadmap update".into(),
                preview: "Thanks for the update. I agree with the proposed sequence...".into(),
                body: "Hi,\n\nThanks for the update. I agree with the proposed sequence and left two comments on the rollout plan. The smaller first milestone should keep risk low while giving us useful feedback.\n\nDaniel".into(),
                time: "Wed".into(),
                date: "Wednesday, 15:04".into(),
                folder: Folder::Inbox,
                unread: false,
                starred: false,
                has_attachment: false,
                accent: (48, 166, 112),
            },
            MailRecord {
                id: 9,
                account_id: WORK_ACCOUNT,
                sender: "Sarah Lee".into(),
                email: "sarah@northstar.studio".into(),
                initials: "SL".into(),
                subject: "Design system feedback".into(),
                preview: "Attached is my feedback on the updated component guidance...".into(),
                body: "Hello,\n\nAttached is my feedback on the updated component guidance. The hierarchy is much clearer now. I marked three places where the keyboard and high-contrast behavior need one more pass.\n\nSarah".into(),
                time: "Tue".into(),
                date: "Tuesday, 11:36".into(),
                folder: Folder::Inbox,
                unread: true,
                starred: false,
                has_attachment: true,
                accent: (136, 83, 208),
            },
            MailRecord {
                id: 10,
                account_id: PERSONAL_ACCOUNT,
                sender: "Alex Thompson".into(),
                email: "alex@example.com".into(),
                initials: "AT".into(),
                subject: "Meeting recap: Community sync".into(),
                preview: "Here are the decisions and owners from yesterday's call...".into(),
                body: "Hi everyone,\n\nHere are the decisions and owners from yesterday's call. We will keep the September date, confirm the venue next week, and send the first agenda draft before the end of the month.\n\nAlex".into(),
                time: "Mon".into(),
                date: "Monday, 17:18".into(),
                folder: Folder::Inbox,
                unread: true,
                starred: false,
                has_attachment: false,
                accent: (216, 92, 35),
            },
            MailRecord {
                id: 11,
                account_id: WORK_ACCOUNT,
                sender: "Nina Brooks".into(),
                email: "nina@ledger.example".into(),
                initials: "NB".into(),
                subject: "Invoice #INV-2026-0718".into(),
                preview: "Please see the attached invoice for this month's services...".into(),
                body: "Hello,\n\nPlease see the attached invoice for this month's services. Payment terms and remittance details are included in the document.\n\nRegards,\nNina".into(),
                time: "Mon".into(),
                date: "Monday, 09:20".into(),
                folder: Folder::Inbox,
                unread: false,
                starred: false,
                has_attachment: true,
                accent: (0, 150, 160),
            },
            MailRecord {
                id: 12,
                account_id: PERSONAL_ACCOUNT,
                sender: "Team Support".into(),
                email: "support@example.com".into(),
                initials: "TS".into(),
                subject: "Your security alert".into(),
                preview: "We noticed a new sign-in to your account from a trusted device...".into(),
                body: "Hello,\n\nWe noticed a new sign-in to your account from a trusted device. No action is required if this was you. You can review recent activity from account settings.\n\nTeam Support".into(),
                time: "Sun".into(),
                date: "Sunday, 18:42".into(),
                folder: Folder::Inbox,
                unread: false,
                starred: false,
                has_attachment: false,
                accent: (217, 164, 0),
            },
            MailRecord {
                id: 6,
                account_id: WORK_ACCOUNT,
                sender: "You".into(),
                email: "you@nivalis.local".into(),
                initials: "YO".into(),
                subject: "Quarterly planning notes".into(),
                preview: "Sharing the notes and decisions from our planning session...".into(),
                body: "Sharing the notes and decisions from our planning session. The main objective is to reduce coordination cost without hiding important context.".into(),
                time: "Mon".into(),
                date: "Monday, 14:21".into(),
                folder: Folder::Sent,
                unread: false,
                starred: false,
                has_attachment: true,
                accent: (46, 107, 88),
            },
            MailRecord {
                id: 7,
                account_id: PERSONAL_ACCOUNT,
                sender: "Draft".into(),
                email: "".into(),
                initials: "DR".into(),
                subject: "Ideas for the autumn workshop".into(),
                preview: "A half-day format could give everyone enough room to make something...".into(),
                body: "A half-day format could give everyone enough room to make something concrete while keeping the group small.".into(),
                time: "Draft".into(),
                date: "Edited yesterday".into(),
                folder: Folder::Drafts,
                unread: false,
                starred: false,
                has_attachment: false,
                accent: (201, 130, 43),
            },
        ];

        Self {
            messages,
            active_account_id: ALL_ACCOUNTS,
            active_folder: Folder::Inbox,
            query: String::new(),
            selected_id: Some(1),
            #[cfg(test)]
            next_id: 13,
            last_deleted: None,
            account_labels: [
                "Unknown".into(),
                ACCOUNT_PROFILES[0].name.into(),
                ACCOUNT_PROFILES[1].name.into(),
                ACCOUNT_PROFILES[2].name.into(),
            ],
        }
    }

    #[cfg(test)]
    pub fn accounts(&self) -> Vec<AccountItem> {
        self.accounts_with_stats(&self.stats())
    }

    pub fn accounts_with_stats(&self, stats: &MailStats) -> Vec<AccountItem> {
        let mut accounts = Vec::with_capacity(ACCOUNT_PROFILES.len() + 1);
        accounts.push(AccountItem {
            id: encode_account_id(ALL_ACCOUNTS),
            name: "All inboxes".into(),
            address: "3 local sample accounts".into(),
            initials: "AI".into(),
            unread_count: stats.account_unread[0],
            status: "Sample data: 1 account warning".into(),
            avatar_color: Color::from_rgb_u8(51, 82, 68),
            has_error: ACCOUNT_PROFILES.iter().any(|account| account.has_error),
        });
        accounts.extend(
            ACCOUNT_PROFILES
                .into_iter()
                .enumerate()
                .map(|(index, account)| AccountItem {
                    id: encode_account_id(account.id),
                    name: account.name.into(),
                    address: account.address.into(),
                    initials: account.initials.into(),
                    unread_count: stats.account_unread[index + 1],
                    status: account.status.into(),
                    avatar_color: color(account.accent),
                    has_error: account.has_error,
                }),
        );
        accounts
    }

    pub fn active_account_id(&self) -> i32 {
        self.active_account_id
    }

    pub fn active_account_name(&self) -> &'static str {
        self.active_profile()
            .map_or("All inboxes", |account| account.name)
    }

    pub fn active_account_detail(&self) -> &'static str {
        self.active_profile()
            .map_or("3 local sample accounts", |account| account.address)
    }

    pub fn active_account_initials(&self) -> &'static str {
        self.active_profile()
            .map_or("AI", |account| account.initials)
    }

    pub fn active_account_color(&self) -> Color {
        self.active_profile().map_or_else(
            || Color::from_rgb_u8(51, 82, 68),
            |account| color(account.accent),
        )
    }

    pub fn active_account_error(&self) -> bool {
        self.active_profile().map_or_else(
            || ACCOUNT_PROFILES.iter().any(|account| account.has_error),
            |account| account.has_error,
        )
    }

    pub fn set_account(&mut self, account_id: i32) -> bool {
        if account_id != ALL_ACCOUNTS && account_profile(account_id).is_none() {
            return false;
        }

        self.active_account_id = account_id;
        let selected_id = self.filtered_records().next().map(|mail| mail.id);
        self.selected_id = selected_id;
        true
    }

    pub fn active_folder(&self) -> &'static str {
        self.active_folder.name()
    }

    pub fn selected_id(&self) -> Option<i32> {
        self.selected_id
    }

    pub fn set_folder(&mut self, folder: &str) {
        let Some(folder) = Folder::from_name(folder) else {
            return;
        };
        self.active_folder = folder;
        let selected_id = self.filtered_records().next().map(|mail| mail.id);
        self.selected_id = selected_id;
    }

    pub fn set_query(&mut self, query: &str) {
        let query = query.trim();
        if self.query != query {
            self.query.clear();
            self.query.push_str(query);
        }
        self.ensure_selection_visible();
    }

    pub fn select(&mut self, id: i32) {
        if let Some(mail) = self.messages.iter_mut().find(|mail| mail.id == id) {
            mail.unread = false;
            self.selected_id = Some(id);
        }
    }

    pub fn toggle_star(&mut self, id: i32) {
        if let Some(mail) = self.messages.iter_mut().find(|mail| mail.id == id) {
            mail.starred = !mail.starred;
        }
        self.ensure_selection_visible();
    }

    pub fn archive(&mut self, id: i32) {
        self.move_to_folder(id, Folder::Archive);
    }

    pub fn delete(&mut self, id: i32) -> bool {
        let visible_before = self.visible_ids();
        let selected_index = visible_before
            .iter()
            .position(|visible_id| *visible_id == id);
        let Some(message_index) = self.messages.iter().position(|mail| mail.id == id) else {
            return false;
        };

        if self.messages[message_index].folder == Folder::Trash {
            self.last_deleted = None;
            self.messages.remove(message_index);
            self.select_near_removed(id, selected_index);
            return false;
        }

        let previous_folder =
            std::mem::replace(&mut self.messages[message_index].folder, Folder::Trash);
        self.last_deleted = Some(DeletedMail {
            id,
            previous_folder,
        });
        self.select_near_removed(id, selected_index);
        true
    }

    pub fn undo_delete(&mut self) -> Option<i32> {
        let deleted = self.last_deleted.take()?;
        let mail = self
            .messages
            .iter_mut()
            .find(|mail| mail.id == deleted.id)?;
        if mail.folder != Folder::Trash {
            return None;
        }

        mail.folder = deleted.previous_folder;
        self.ensure_selection_visible();
        Some(deleted.id)
    }

    pub fn mark_unread(&mut self, id: i32) {
        if let Some(mail) = self.messages.iter_mut().find(|mail| mail.id == id) {
            mail.unread = true;
        }
    }

    #[cfg(test)]
    pub fn insert_test_sent_mail(&mut self, recipient: &str, subject: &str, body: &str) -> i32 {
        let id = self.next_id;
        self.next_id += 1;
        let normalized_body = body.trim();
        let account_id = if self.active_account_id == ALL_ACCOUNTS {
            WORK_ACCOUNT
        } else {
            self.active_account_id
        };
        let accent = account_profile(account_id)
            .map(|account| account.accent)
            .unwrap_or((50, 96, 78));
        let mail = MailRecord {
            id,
            account_id,
            sender: "You".into(),
            email: recipient.trim().into(),
            initials: "YO".into(),
            subject: subject.trim().into(),
            preview: first_line_preview(normalized_body),
            body: normalized_body.into(),
            time: "Now".into(),
            date: "Just now".into(),
            folder: Folder::Sent,
            unread: false,
            starred: false,
            has_attachment: false,
            accent,
        };
        self.messages.insert(0, mail);
        id
    }

    #[cfg(test)]
    pub fn filtered(&self) -> Vec<MailSummary> {
        self.view().rows
    }

    #[cfg(test)]
    pub fn filtered_count(&self) -> i32 {
        self.stats().message_total
    }

    pub fn view(&self) -> MailView {
        self.collect_view(true)
    }

    pub fn stats(&self) -> MailStats {
        self.collect_view(false).stats
    }

    pub fn visible_mail(&self, id: i32) -> Option<(usize, MailSummary)> {
        self.filtered_records()
            .take(PAGE_SIZE)
            .enumerate()
            .find(|(_, mail)| mail.id == id)
            .map(|(index, mail)| (index, self.to_summary(mail)))
    }

    pub fn selected(&self) -> MailDetail {
        self.selected_id
            .and_then(|id| self.messages.iter().find(|mail| mail.id == id))
            .map(MailRecord::to_detail)
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn inbox_count(&self) -> i32 {
        self.stats().inbox_count
    }

    #[cfg(test)]
    pub fn starred_count(&self) -> i32 {
        self.stats().starred_count
    }

    #[cfg(test)]
    pub fn draft_count(&self) -> i32 {
        self.stats().draft_count
    }

    fn filtered_records(&self) -> impl Iterator<Item = &MailRecord> {
        self.messages
            .iter()
            .filter(|mail| self.record_matches(mail))
    }

    fn ensure_selection_visible(&mut self) {
        let visible = self.visible_ids();
        if !self
            .selected_id
            .is_some_and(|selected| visible.contains(&selected))
        {
            self.selected_id = visible.first().copied();
        }
    }

    fn move_to_folder(&mut self, id: i32, target: Folder) {
        let visible_before = self.visible_ids();
        let selected_index = visible_before
            .iter()
            .position(|visible_id| *visible_id == id);

        let Some(mail) = self.messages.iter_mut().find(|mail| mail.id == id) else {
            return;
        };
        if mail.folder == target {
            return;
        }
        mail.folder = target;

        if self.selected_id == Some(id) {
            let visible_after = self.visible_ids();
            self.selected_id = selected_index
                .and_then(|index| visible_after.get(index).or_else(|| visible_after.last()))
                .copied()
                .or_else(|| visible_after.first().copied());
        }
    }

    fn visible_ids(&self) -> Vec<i32> {
        self.filtered_records()
            .take(PAGE_SIZE)
            .map(|mail| mail.id)
            .collect()
    }

    fn active_profile(&self) -> Option<&'static AccountProfile> {
        account_profile(self.active_account_id)
    }

    fn account_matches(&self, mail: &MailRecord) -> bool {
        self.active_account_id == ALL_ACCOUNTS || mail.account_id == self.active_account_id
    }

    fn record_matches(&self, mail: &MailRecord) -> bool {
        self.account_matches(mail)
            && match self.active_folder {
                Folder::Starred => mail.starred && mail.folder != Folder::Trash,
                Folder::Unread => mail.folder == Folder::Inbox && mail.unread,
                folder => mail.folder == folder,
            }
            && (self.query.is_empty()
                || contains_ignore_ascii_case(mail.sender.as_str(), &self.query)
                || contains_ignore_ascii_case(mail.subject.as_str(), &self.query)
                || contains_ignore_ascii_case(mail.preview.as_str(), &self.query))
    }

    fn collect_view(&self, collect_rows: bool) -> MailView {
        let mut rows = if collect_rows {
            Vec::with_capacity(PAGE_SIZE.min(self.messages.len()))
        } else {
            Vec::new()
        };
        let mut stats = MailStats::default();

        for mail in &self.messages {
            if mail.folder == Folder::Inbox && mail.unread {
                stats.account_unread[0] += 1;
                if let Some(index) = account_model_index(mail.account_id) {
                    stats.account_unread[index] += 1;
                }
            }

            if self.account_matches(mail) {
                stats.inbox_count += i32::from(mail.folder == Folder::Inbox && mail.unread);
                stats.starred_count += i32::from(mail.starred && mail.folder != Folder::Trash);
                stats.draft_count += i32::from(mail.folder == Folder::Drafts);
            }

            if self.record_matches(mail) {
                stats.message_total += 1;
                if collect_rows && rows.len() < PAGE_SIZE {
                    rows.push(self.to_summary(mail));
                }
            }
        }

        MailView { rows, stats }
    }

    fn to_summary(&self, mail: &MailRecord) -> MailSummary {
        let label = account_model_index(mail.account_id)
            .and_then(|index| self.account_labels.get(index))
            .cloned()
            .unwrap_or_else(|| self.account_labels[0].clone());
        mail.to_summary(label)
    }

    fn select_near_removed(&mut self, id: i32, selected_index: Option<usize>) {
        if self.selected_id != Some(id) {
            return;
        }

        let visible_after = self.visible_ids();
        self.selected_id = selected_index
            .and_then(|index| visible_after.get(index).or_else(|| visible_after.last()))
            .copied()
            .or_else(|| visible_after.first().copied());
    }
}

fn account_profile(id: i32) -> Option<&'static AccountProfile> {
    ACCOUNT_PROFILES.iter().find(|account| account.id == id)
}

fn account_model_index(id: i32) -> Option<usize> {
    if id == ALL_ACCOUNTS {
        return Some(0);
    }
    ACCOUNT_PROFILES
        .iter()
        .position(|account| account.id == id)
        .map(|index| index + 1)
}

fn entity_key(id: i32) -> EntityKey {
    EntityKey::new(i64::from(id)).expect("in-memory entity IDs are positive")
}

fn encode_entity_id(id: i32) -> SharedString {
    entity_key(id).encode()
}

fn encode_account_id(id: i32) -> SharedString {
    if id == ALL_ACCOUNTS {
        AccountKey::All.encode()
    } else {
        AccountKey::Account(entity_key(id)).encode()
    }
}

fn color(rgb: (u8, u8, u8)) -> Color {
    Color::from_rgb_u8(rgb.0, rgb.1, rgb.2)
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }

    let needle = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
fn first_line_preview(body: &str) -> SharedString {
    let line = body.lines().next().unwrap_or_default().trim();
    let Some((end, _)) = line.char_indices().nth(PREVIEW_MAX_CHARS) else {
        return line.into();
    };

    let mut preview = String::with_capacity(end + 3);
    preview.push_str(&line[..end]);
    preview.push_str("...");
    preview.into()
}

fn bounded_text(text: &SharedString, max_chars: usize) -> (SharedString, bool) {
    let Some((end, _)) = text.char_indices().nth(max_chars) else {
        return (text.clone(), false);
    };

    (text[..end].into(), true)
}

#[cfg(test)]
mod tests {
    use super::{
        BODY_PREVIEW_MAX_CHARS, MailStore, PAGE_SIZE, PREVIEW_MAX_CHARS, encode_entity_id,
    };

    #[test]
    fn search_matches_sender_and_subject_case_insensitively() {
        let mut store = MailStore::demo();
        store.set_query("ARCHITECTURE");

        let result = store.filtered();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "2");
    }

    #[test]
    fn visible_model_is_capped_to_one_summary_page() {
        let mut store = MailStore::demo();
        store.set_folder("Sent");
        let existing = store.filtered_count();

        for index in 0..75 {
            store.insert_test_sent_mail(
                "recipient@example.com",
                &format!("Bounded page {index}"),
                "The full body stays in the store, not in each visible summary.",
            );
        }

        assert_eq!(store.filtered().len(), PAGE_SIZE);
        assert_eq!(store.filtered_count(), existing + 75);
    }

    #[test]
    fn capped_page_refills_after_delete_and_undo() {
        let mut store = MailStore::demo();
        store.set_folder("Sent");
        let mut newest_id = 0;
        for index in 0..75 {
            newest_id = store.insert_test_sent_mail(
                "recipient@example.com",
                &format!("Bounded page {index}"),
                "A bounded summary body.",
            );
        }

        let before = store.view();
        let newest_key = encode_entity_id(newest_id);
        assert_eq!(before.rows.len(), PAGE_SIZE);
        assert_eq!(before.rows[0].id, newest_key);

        assert!(store.delete(newest_id));
        let deleted = store.view();
        assert_eq!(deleted.rows.len(), PAGE_SIZE);
        assert_eq!(deleted.stats.message_total, before.stats.message_total - 1);
        assert!(deleted.rows.iter().all(|mail| mail.id != newest_key));

        assert_eq!(store.undo_delete(), Some(newest_id));
        let restored = store.view();
        assert_eq!(restored.rows.len(), PAGE_SIZE);
        assert_eq!(restored.stats.message_total, before.stats.message_total);
        assert_eq!(restored.rows[0].id, newest_key);
    }

    #[test]
    fn summaries_and_details_share_stable_text_storage() {
        let store = MailStore::demo();
        let summary = store.view().rows.remove(0);
        let detail = store.selected();

        assert_eq!(summary.id, "1");
        assert_eq!(summary.account_id, "1");
        assert_eq!(detail.id, "1");
        assert_eq!(
            summary.sender.as_str().as_ptr(),
            store.messages[0].sender.as_str().as_ptr()
        );
        assert_eq!(
            detail.body.as_str().as_ptr(),
            store.messages[0].body.as_str().as_ptr()
        );
    }

    #[test]
    fn archiving_moves_message_out_of_inbox() {
        let mut store = MailStore::demo();
        let before = store.filtered().len();

        store.archive(1);

        assert_eq!(store.filtered().len(), before - 1);
        store.set_folder("Archive");
        assert!(store.filtered().iter().any(|mail| mail.id == "1"));
    }

    #[test]
    fn opening_an_unread_message_updates_the_badge() {
        let mut store = MailStore::demo();
        let before = store.inbox_count();

        store.select(2);

        assert_eq!(store.inbox_count(), before - 1);
    }

    #[test]
    fn search_keeps_selection_inside_visible_results() {
        let mut store = MailStore::demo();

        store.set_query("architecture");
        assert_eq!(store.selected_id(), Some(2));

        store.set_query("nothing matches this");
        assert_eq!(store.selected_id(), None);
    }

    #[test]
    fn removing_star_from_selected_result_advances_selection() {
        let mut store = MailStore::demo();
        store.set_folder("Starred");
        store.select(1);

        store.toggle_star(1);

        assert_eq!(store.selected_id(), Some(3));
        assert!(!store.filtered().iter().any(|mail| mail.id == "1"));
    }

    #[test]
    fn deleting_starred_mail_excludes_it_from_virtual_folder() {
        let mut store = MailStore::demo();
        store.set_folder("Starred");
        let before = store.starred_count();

        store.delete(1);

        assert_eq!(store.starred_count(), before - 1);
        assert!(!store.filtered().iter().any(|mail| mail.id == "1"));
    }

    #[test]
    fn archiving_selected_mail_preserves_nearby_reading_position() {
        let mut store = MailStore::demo();
        store.select(3);

        store.archive(3);
        assert_eq!(store.selected_id(), Some(4));

        store.select(5);
        store.archive(5);
        assert_eq!(store.selected_id(), Some(8));
    }

    #[test]
    fn sent_preview_uses_first_line_after_leading_whitespace() {
        let mut store = MailStore::demo();
        let id = store.insert_test_sent_mail(
            "friend@example.com",
            "Hello",
            "\n\n  First line\nSecond line",
        );
        store.set_folder("Sent");
        let key = encode_entity_id(id);

        let sent = store
            .filtered()
            .into_iter()
            .find(|mail| mail.id == key)
            .expect("sent message should be visible");

        assert_eq!(sent.preview.as_str(), "First line");
        store.select(id);
        assert_eq!(store.selected().body.as_str(), "First line\nSecond line");
    }

    #[test]
    fn long_single_line_body_has_a_bounded_utf8_preview() {
        let mut store = MailStore::demo();
        let body = "界".repeat(PREVIEW_MAX_CHARS + 40);
        let id = store.insert_test_sent_mail("friend@example.com", "Long body", &body);
        store.set_folder("Sent");
        let key = encode_entity_id(id);

        let sent = store
            .filtered()
            .into_iter()
            .find(|mail| mail.id == key)
            .expect("sent message should be visible");

        assert_eq!(sent.preview.chars().count(), PREVIEW_MAX_CHARS + 3);
        assert!(sent.preview.ends_with("..."));
        store.select(id);
        assert_eq!(store.selected().body.as_str(), body);
    }

    #[test]
    fn unusually_large_body_is_bounded_in_the_reader_projection() {
        let mut store = MailStore::demo();
        let body = "界".repeat(BODY_PREVIEW_MAX_CHARS + 100);
        let id = store.insert_test_sent_mail("friend@example.com", "Large body", &body);
        store.set_folder("Sent");
        store.select(id);

        let bounded = store.selected();
        assert!(bounded.body_truncated);
        assert_eq!(bounded.body.chars().count(), BODY_PREVIEW_MAX_CHARS);
    }

    #[test]
    fn archiving_an_opened_unread_message_selects_the_next_unread() {
        let mut store = MailStore::demo();
        store.set_folder("Unread");
        store.select(1);

        store.archive(1);

        assert_eq!(store.selected_id(), Some(2));
    }

    #[test]
    fn switching_accounts_scopes_messages_badges_and_drafts() {
        let mut store = MailStore::demo();

        assert_eq!(store.filtered().len(), 10);
        assert_eq!(store.inbox_count(), 4);

        assert!(store.set_account(1));
        assert_eq!(
            store
                .filtered()
                .into_iter()
                .map(|mail| mail.id.to_string())
                .collect::<Vec<_>>(),
            ["1", "2", "5", "8", "9", "11"]
        );
        assert_eq!(store.inbox_count(), 3);
        assert_eq!(store.starred_count(), 1);
        assert_eq!(store.draft_count(), 0);

        assert!(store.set_account(2));
        assert_eq!(
            store
                .filtered()
                .into_iter()
                .map(|mail| mail.id.to_string())
                .collect::<Vec<_>>(),
            ["4", "10", "12"]
        );
        assert_eq!(store.inbox_count(), 1);
        assert_eq!(store.draft_count(), 1);
        assert_eq!(store.selected_id(), Some(4));
    }

    #[test]
    fn invalid_account_switch_keeps_current_scope_and_selection() {
        let mut store = MailStore::demo();
        assert!(store.set_account(3));
        let selected = store.selected_id();

        assert!(!store.set_account(99));
        assert_eq!(store.active_account_id(), 3);
        assert_eq!(store.selected_id(), selected);
        assert_eq!(
            store
                .filtered()
                .into_iter()
                .map(|mail| mail.id.to_string())
                .collect::<Vec<_>>(),
            ["3"]
        );
    }

    #[test]
    fn account_summaries_expose_unread_and_error_state() {
        let store = MailStore::demo();
        let accounts = store.accounts();

        assert_eq!(accounts.len(), 4);
        assert_eq!(accounts[0].id, "");
        assert_eq!(accounts[0].unread_count, 4);
        assert!(accounts[0].has_error);
        assert_eq!(accounts[1].id, "1");
        assert_eq!(accounts[1].name.as_str(), "Work");
        assert_eq!(accounts[1].unread_count, 3);
        assert_eq!(accounts[2].name.as_str(), "Personal");
        assert!(accounts[2].has_error);
    }

    #[test]
    fn deleting_to_trash_can_be_undone_once() {
        let mut store = MailStore::demo();

        assert!(store.delete(1));
        assert!(!store.filtered().iter().any(|mail| mail.id == "1"));
        store.set_folder("Trash");
        assert!(store.filtered().iter().any(|mail| mail.id == "1"));

        assert_eq!(store.undo_delete(), Some(1));
        assert!(!store.filtered().iter().any(|mail| mail.id == "1"));
        assert_eq!(store.undo_delete(), None);

        store.set_folder("Inbox");
        assert!(store.filtered().iter().any(|mail| mail.id == "1"));
    }

    #[test]
    fn deleting_from_trash_is_permanent_and_not_undoable() {
        let mut store = MailStore::demo();
        assert!(store.delete(1));
        store.set_folder("Trash");

        assert!(!store.delete(1));
        assert_eq!(store.undo_delete(), None);
        assert!(!store.filtered().iter().any(|mail| mail.id == "1"));

        store.set_folder("Inbox");
        assert!(!store.filtered().iter().any(|mail| mail.id == "1"));
    }

    #[test]
    fn sent_mail_uses_the_active_account() {
        let mut store = MailStore::demo();
        assert!(store.set_account(2));
        let id = store.insert_test_sent_mail("friend@example.com", "Hello", "From personal");
        store.set_folder("Sent");
        let key = encode_entity_id(id);

        let sent = store
            .filtered()
            .into_iter()
            .find(|mail| mail.id == key)
            .expect("sent message should be visible in the active account");
        assert_eq!(sent.account_id, "2");
        assert_eq!(sent.account_label.as_str(), "Personal");

        assert!(store.set_account(1));
        assert!(!store.filtered().iter().any(|mail| mail.id == key));
    }
}
